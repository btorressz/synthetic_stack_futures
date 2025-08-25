use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::AssociatedToken,
    token::{self, CloseAccount, Mint, Token, TokenAccount, Transfer},
};

declare_id!("FSBdeh58ourJm9Wjf1BFZ8jSGrgbhN2jrF3Vw4BdiQx1");

/// Synthetic Stack Futures (cash-settled)
/// PoC features:
/// - Market admin + simple multisig + timelock
/// - Oracle NAV with freshness + jump-limit + optional confidence gate
/// - Bilateral deal with margin, fees, liquidation, partial liquidation to IM
/// - Leverage caps (at open and as a liquidation trigger)
/// - Socialized loss floor: pause on vault depletion (PoC safeguard)

pub const UNIT_DECIMALS: u8 = 6; // size units precision (1e6)
pub const VERSION_SEED: &[u8] = b"v1";
pub const MAX_ADMINS: usize = 5;

#[program]
pub mod synthetic_stack_futures {
    use super::*;

    // ──────────────────────────────────────────────────────────────────────────────
    // Admin / Market
    // ──────────────────────────────────────────────────────────────────────────────

    pub fn init_market(
        ctx: Context<InitMarket>,
        stack_id: Pubkey,
        params: MarketInitParams,
    ) -> Result<()> {
        let market = &mut ctx.accounts.market;

        market.authority = ctx.accounts.authority.key();
        market.quote_mint = ctx.accounts.quote_mint.key();
        market.oracle_authority = params.oracle_authority;
        market.stack_id = stack_id;

        market.price_decimals = params.price_decimals;
        market.quote_decimals = ctx.accounts.quote_mint.decimals;

        market.initial_margin_bps = params.initial_margin_bps;
        market.maintenance_margin_bps = params.maintenance_margin_bps;
        market.fee_bps = params.fee_bps;
        market.liquidator_bps = params.liquidator_bps;
        market.price_stale_seconds = params.price_stale_seconds;

        // New risk/admin fields
        market.max_leverage_bps = params.max_leverage_bps;
        market.max_nav_jump_bps = params.max_nav_jump_bps;
        market.max_confidence_bps = params.max_confidence_bps.unwrap_or(0);
        market.mm_buffer_bps = params.mm_buffer_bps.unwrap_or(100); // 1% default
        market.circuit_breaker_until = 0;

        // Multisig defaults (PoC: authority is admin[0], threshold = 1 or provided)
        market.admin_threshold = params.admin_threshold.unwrap_or(1);
        market.admins = [Pubkey::default(); MAX_ADMINS];
        market.admins[0] = market.authority;

        market.last_nav = 0;
        market.last_ts = 0;
        market.paused = false;
        market.bump = ctx.bumps.market;
        market.pending = None;

        let mva = &mut ctx.accounts.market_vault_auth;
        mva.market = market.key();
        mva.bump = ctx.bumps.market_vault_auth;

        emit!(MarketInitialized {
            market: market.key(),
            quote_mint: market.quote_mint,
            stack_id,
            im_bps: market.initial_margin_bps,
            mm_bps: market.maintenance_margin_bps,
            fee_bps: market.fee_bps,
            liq_bps: market.liquidator_bps,
            price_decimals: market.price_decimals,
            quote_decimals: market.quote_decimals,
        });

        Ok(())
    }

    pub fn pause_market(ctx: Context<AdminMarketToggle>, paused: bool) -> Result<()> {
        // FIX: avoid lifetime coupling by passing key + remaining infos
        require_admin_or_multisig(&ctx.accounts.market, ctx.accounts.authority.key(), &ctx.remaining_accounts)?;
        ctx.accounts.market.paused = paused;
        Ok(())
    }

    pub fn update_market_params(ctx: Context<AdminMarketParams>, params: MarketUpdateParams) -> Result<()> {
        require_admin_or_multisig(&ctx.accounts.market, ctx.accounts.authority.key(), &ctx.remaining_accounts)?;
        apply_market_updates(&mut ctx.accounts.market, &params);
        Ok(())
    }

    /// Propose market params (timelocked)
    pub fn propose_market_params(
        ctx: Context<AdminMarketParams>,
        params: MarketUpdateParams,
        delay_secs: i64,
    ) -> Result<()> {
        require_admin_or_multisig(&ctx.accounts.market, ctx.accounts.authority.key(), &ctx.remaining_accounts)?;
        let now = Clock::get()?.unix_timestamp;
        ctx.accounts.market.pending = Some(PendingParams { params, eta: now + delay_secs });
        Ok(())
    }

    /// Execute pending market params after ETA
    pub fn execute_market_params(ctx: Context<AdminMarketParams>) -> Result<()> {
        require_admin_or_multisig(&ctx.accounts.market, ctx.accounts.authority.key(), &ctx.remaining_accounts)?;
        let now = Clock::get()?.unix_timestamp;
        let Some(p) = ctx.accounts.market.pending.clone() else { return err!(ErrorCode::NoPendingParams); };
        require!(now >= p.eta, ErrorCode::TimelockNotExpired);
        apply_market_updates(&mut ctx.accounts.market, &p.params);
        ctx.accounts.market.pending = None;
        Ok(())
    }

    /// Rotate authority (multisig or authority)
    pub fn rotate_authority(ctx: Context<AdminMarketParams>, new_authority: Pubkey) -> Result<()> {
        require_admin_or_multisig(&ctx.accounts.market, ctx.accounts.authority.key(), &ctx.remaining_accounts)?;
        ctx.accounts.market.authority = new_authority;
        // PoC: also update admins[0] to keep UX simple
        ctx.accounts.market.admins[0] = new_authority;
        Ok(())
    }

    // Oracle posts NAV (scaled by market.price_decimals). Optional confidence gate.
    pub fn post_nav(ctx: Context<PostNav>, nav: u64, nav_confidence: Option<u64>) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require!(!market.paused, ErrorCode::MarketPaused);
        require_keys_eq!(market.oracle_authority, ctx.accounts.oracle_authority.key(), ErrorCode::Unauthorized);

        // Circuit breaker window check
        let now = Clock::get()?.unix_timestamp;
        if now < market.circuit_breaker_until {
            return err!(ErrorCode::CircuitBreaker);
        }

        // Confidence (if configured and provided)
        if market.max_confidence_bps > 0 {
            if let Some(conf) = nav_confidence {
                let conf_bps = ratio_bps_u128(conf as u128, (nav as u128).max(1))? as u16;
                require!(conf_bps <= market.max_confidence_bps, ErrorCode::OracleConfidenceTooWide);
            }
        }

        // Jump limit check
        if market.last_nav != 0 {
            let old = market.last_nav as u128;
            let newv = nav as u128;
            let diff = if newv > old { newv - old } else { old - newv };
            let jump_bps = ratio_bps_u128(diff, old.max(1))? as u16;
            if jump_bps > market.max_nav_jump_bps {
                // Trip circuit breaker for a short cool-off (PoC: 5 minutes)
                market.circuit_breaker_until = now + 300;
                return err!(ErrorCode::PriceJumpTooLarge);
            }
        }

        market.last_nav = nav;
        market.last_ts = now;

        emit!(NavPosted { market: market.key(), nav, ts: market.last_ts });
        Ok(())
    }

    // ──────────────────────────────────────────────────────────────────────────────
    // Trading (Bilateral Deal)
    // ──────────────────────────────────────────────────────────────────────────────

    /// Open a bilateral futures deal.
    /// - size: stack units scaled by 1e6 (UNIT_DECIMALS)
    /// - client_order_id: disambiguates multiple deals between same parties
    /// - long_deposit / short_deposit: quote-mint amounts to move into margin vaults
    pub fn open_deal(
        ctx: Context<OpenDeal>,
        client_order_id: u64,
        size: u64,
        long_deposit: u64,
        short_deposit: u64,
    ) -> Result<()> {
        let market = &ctx.accounts.market;
        require!(!market.paused, ErrorCode::MarketPaused);
        require!(size > 0, ErrorCode::ZeroSize);
        ensure_price_fresh(market)?;

        // Entry price and notional (in quote mint decimals)
        let entry_nav = market.last_nav;
        let notional_q = notional_quote(size, entry_nav, market.price_decimals, market.quote_decimals)?;

        // Fees & margin requirements
        let open_fee_total = bps(notional_q, market.fee_bps)?;
        let open_fee_each = open_fee_total / 2;
        let im_required_each = bps(notional_q, market.initial_margin_bps)?;

        require!(long_deposit as u128 >= im_required_each + open_fee_each, ErrorCode::InsufficientMargin);
        require!(short_deposit as u128 >= im_required_each + open_fee_each, ErrorCode::InsufficientMargin);

        // Leverage cap at open: based on total effective margin after fees
        let effective_total_margin = (long_deposit as u128)
            .saturating_add(short_deposit as u128)
            .saturating_sub(open_fee_total);
        require!(effective_total_margin > 0, ErrorCode::InsufficientMargin);
        let lev_bps = ratio_bps_u128(notional_q, effective_total_margin)? as u16;
        require!(lev_bps <= market.max_leverage_bps, ErrorCode::LeverageTooHigh);

        // Init deal PDA
        let deal = &mut ctx.accounts.deal;
        require!(!deal.is_open, ErrorCode::AlreadyOpen);
        deal.market = market.key();
        deal.long = ctx.accounts.long.key();
        deal.short = ctx.accounts.short.key();
        deal.size = size;
        deal.entry_nav = entry_nav;
        deal.is_open = true;
        deal.long_margin = 0;
        deal.short_margin = 0;
        deal.client_order_id = client_order_id;
        deal.bump = ctx.bumps.deal;

        // Init deal vault auth PDA
        let dva = &mut ctx.accounts.deal_vault_auth;
        dva.deal = deal.key();
        dva.bump = ctx.bumps.deal_vault_auth;

        // Move deposits from users to their margin vaults
        transfer_from_user(
            &ctx.accounts.token_program,
            &ctx.accounts.long_source,
            &ctx.accounts.long_margin_vault,
            &ctx.accounts.long,
            long_deposit,
        )?;
        transfer_from_user(
            &ctx.accounts.token_program,
            &ctx.accounts.short_source,
            &ctx.accounts.short_margin_vault,
            &ctx.accounts.short,
            short_deposit,
        )?;

        // Collect open fees from vaults to market fee_vault (authority = deal_vault_auth PDA)
        let deal_key = deal.key();
        let seeds: [&[u8]; 4] = [VERSION_SEED, b"deal_vault_auth", deal_key.as_ref(), &[dva.bump]];
        transfer_signed(
            &ctx.accounts.token_program,
            &ctx.accounts.long_margin_vault,
            &ctx.accounts.fee_vault,
            ctx.accounts.deal_vault_auth.to_account_info(),
            &seeds[..],
            open_fee_each.try_into().unwrap(),
        )?;
        transfer_signed(
            &ctx.accounts.token_program,
            &ctx.accounts.short_margin_vault,
            &ctx.accounts.fee_vault,
            ctx.accounts.deal_vault_auth.to_account_info(),
            &seeds[..],
            open_fee_each.try_into().unwrap(),
        )?;

        // Update stored margin balances
        deal.long_margin = ctx.accounts.long_margin_vault.amount;
        deal.short_margin = ctx.accounts.short_margin_vault.amount;

        emit!(DealOpened {
            deal: deal.key(),
            market: deal.market,
            long: deal.long,
            short: deal.short,
            size,
            entry_nav,
            notional_quote: notional_q as u64,
            long_deposit,
            short_deposit,
            open_fee_each: open_fee_each as u64,
        });

        Ok(())
    }

    /// Add margin for the LONG side.
    pub fn add_margin_long(ctx: Context<AddMarginLong>, amount: u64) -> Result<()> {
        require!(ctx.accounts.deal.is_open, ErrorCode::NotOpen);
        require_keys_eq!(ctx.accounts.deal.long, ctx.accounts.long.key(), ErrorCode::Unauthorized);
        transfer_from_user(
            &ctx.accounts.token_program,
            &ctx.accounts.long_source,
            &ctx.accounts.long_margin_vault,
            &ctx.accounts.long,
            amount,
        )?;
        ctx.accounts.deal.long_margin = ctx.accounts.long_margin_vault.amount;
        Ok(())
    }

    /// Add margin for the SHORT side.
    pub fn add_margin_short(ctx: Context<AddMarginShort>, amount: u64) -> Result<()> {
        require!(ctx.accounts.deal.is_open, ErrorCode::NotOpen);
        require_keys_eq!(ctx.accounts.deal.short, ctx.accounts.short.key(), ErrorCode::Unauthorized);
        transfer_from_user(
            &ctx.accounts.token_program,
            &ctx.accounts.short_source,
            &ctx.accounts.short_margin_vault,
            &ctx.accounts.short,
            amount,
        )?;
        ctx.accounts.deal.short_margin = ctx.accounts.short_margin_vault.amount;
        Ok(())
    }

    /// Close the deal at current NAV; pays both sides and closes vaults.
    pub fn close_deal(ctx: Context<CloseDeal>) -> Result<()> {
        let market = &ctx.accounts.market;
        let deal = &mut ctx.accounts.deal;
        require!(deal.is_open, ErrorCode::NotOpen);
        require!(!market.paused, ErrorCode::MarketPaused);
        ensure_price_fresh(market)?;

        let long_v = &ctx.accounts.long_margin_vault;
        let short_v = &ctx.accounts.short_margin_vault;
        let total_pool = (long_v.amount as u128) + (short_v.amount as u128);

        // PnL for LONG in quote units (signed)
        let pnl_long = pnl_quote(
            deal.size,
            deal.entry_nav,
            market.last_nav,
            market.price_decimals,
            market.quote_decimals,
        )?;

        // Desired payouts before clamping
        let desired_long = (long_v.amount as i128) + pnl_long;
        let long_payout = clamp_i128(desired_long, 0, total_pool as i128) as u128;
        let short_payout = total_pool.saturating_sub(long_payout);

        // Payouts (drain vaults)
        drain_to(
            &ctx.accounts.token_program,
            &ctx.accounts.long_margin_vault,
            &ctx.accounts.long_payout_ata,
            &ctx.accounts.deal_vault_auth,
            &deal,
            long_payout as u64,
        )?;
        drain_to(
            &ctx.accounts.token_program,
            &ctx.accounts.short_margin_vault,
            &ctx.accounts.short_payout_ata,
            &ctx.accounts.deal_vault_auth,
            &deal,
            short_payout as u64,
        )?;

        // Close empty vaults back to market authority (receives rent)
        close_signed_token_account(
            &ctx.accounts.token_program,
            &ctx.accounts.long_margin_vault,
            &ctx.accounts.market_authority,
            &ctx.accounts.deal_vault_auth,
            &deal,
        )?;
        close_signed_token_account(
            &ctx.accounts.token_program,
            &ctx.accounts.short_margin_vault,
            &ctx.accounts.market_authority,
            &ctx.accounts.deal_vault_auth,
            &deal,
        )?;

        deal.is_open = false;

        emit!(DealClosed {
            deal: deal.key(),
            market: deal.market,
            long_payout: long_payout as u64,
            short_payout: short_payout as u64,
            close_nav: market.last_nav,
        });

        Ok(())
    }

    /// Liquidate if maintenance breached OR leverage > cap; pays bounty then settle like close.
    pub fn liquidate(ctx: Context<Liquidate>) -> Result<()> {
        let m = &mut ctx.accounts.market;
        let d = &mut ctx.accounts.deal;
        require!(d.is_open, ErrorCode::NotOpen);
        require!(!m.paused, ErrorCode::MarketPaused);
        ensure_price_fresh(m)?;

        let notional_q = notional_quote(d.size, m.last_nav, m.price_decimals, m.quote_decimals)?;
        let mm_required = bps(notional_q, m.maintenance_margin_bps.saturating_add(m.mm_buffer_bps))?;

        // PnL & equity
        let pnl_long = pnl_quote(d.size, d.entry_nav, m.last_nav, m.price_decimals, m.quote_decimals)?;
        let long_eq = (ctx.accounts.long_margin_vault.amount as i128) + pnl_long;
        let short_eq = (ctx.accounts.short_margin_vault.amount as i128) - pnl_long;

        let pool = (ctx.accounts.long_margin_vault.amount as u128)
            .saturating_add(ctx.accounts.short_margin_vault.amount as u128);
        let lev_bps = if pool > 0 { ratio_bps_u128(notional_q, pool)? as u16 } else { u16::MAX };
        let over_lev = lev_bps > m.max_leverage_bps;

        // Liquidatable if either equity < MM or over leverage
        require!(
            long_eq < mm_required as i128 || short_eq < mm_required as i128 || over_lev,
            ErrorCode::NotLiquidatable
        );

        // Bounty from pool
        let bounty = bps(pool, m.liquidator_bps)? as u64;
        if bounty > 0 {
            // pay from long first then short
            let mut remaining = bounty;
            let take_long = remaining.min(ctx.accounts.long_margin_vault.amount);
            if take_long > 0 {
                drain_to(
                    &ctx.accounts.token_program,
                    &ctx.accounts.long_margin_vault,
                    &ctx.accounts.liquidator_ata,
                    &ctx.accounts.deal_vault_auth,
                    &d,
                    take_long,
                )?;
                remaining -= take_long;
            }
            if remaining > 0 {
                drain_to(
                    &ctx.accounts.token_program,
                    &ctx.accounts.short_margin_vault,
                    &ctx.accounts.liquidator_ata,
                    &ctx.accounts.deal_vault_auth,
                    &d,
                    remaining,
                )?;
            }
        }

        // Recompute pool after bounty
        let long_amt = ctx.accounts.long_margin_vault.amount as u128;
        let short_amt = ctx.accounts.short_margin_vault.amount as u128;
        let new_pool = long_amt + short_amt;

        let desired_long = (long_amt as i128) + pnl_long;
        let long_payout = clamp_i128(desired_long, 0, new_pool as i128) as u128;
        let short_payout = new_pool.saturating_sub(long_payout);

        if long_payout > 0 {
            drain_to(
                &ctx.accounts.token_program,
                &ctx.accounts.long_margin_vault,
                &ctx.accounts.long_payout_ata,
                &ctx.accounts.deal_vault_auth,
                &d,
                long_payout as u64,
            )?;
        }
        if short_payout > 0 {
            drain_to(
                &ctx.accounts.token_program,
                &ctx.accounts.short_margin_vault,
                &ctx.accounts.short_payout_ata,
                &ctx.accounts.deal_vault_auth,
                &d,
                short_payout as u64,
            )?;
        }

        // Check depletion before closing
        let depleted = ctx.accounts.long_margin_vault.amount == 0 || ctx.accounts.short_margin_vault.amount == 0;

        // Close vaults
        close_signed_token_account(
            &ctx.accounts.token_program,
            &ctx.accounts.long_margin_vault,
            &ctx.accounts.market_authority,
            &ctx.accounts.deal_vault_auth,
            &d,
        )?;
        close_signed_token_account(
            &ctx.accounts.token_program,
            &ctx.accounts.short_margin_vault,
            &ctx.accounts.market_authority,
            &ctx.accounts.deal_vault_auth,
            &d,
        )?;

        d.is_open = false;

        // Socialized loss floor (PoC): if a vault depleted during liquidation, pause market
        if depleted {
            m.paused = true;
        }

        emit!(DealLiquidated { deal: d.key(), market: d.market, bounty_paid: bounty, close_nav: m.last_nav });
        Ok(())
    }

    /// Partial liquidation: move just enough to bring the under-margined side back to **initial** margin.
    /// Rewards liquidator with bounty on the skimmed amount. Keeps deal open if successful.
    pub fn liquidate_to_im(ctx: Context<PartialLiquidate>, max_bounty_take: u64) -> Result<()> {
        let m = &mut ctx.accounts.market;
        let d = &mut ctx.accounts.deal;
        require!(d.is_open, ErrorCode::NotOpen);
        require!(!m.paused, ErrorCode::MarketPaused);
        ensure_price_fresh(m)?;

        let notional_q = notional_quote(d.size, m.last_nav, m.price_decimals, m.quote_decimals)?;
        let im_required = bps(notional_q, m.initial_margin_bps)? as i128;

        let pnl_long = pnl_quote(d.size, d.entry_nav, m.last_nav, m.price_decimals, m.quote_decimals)?;
        let long_eq = (ctx.accounts.long_margin_vault.amount as i128) + pnl_long;
        let short_eq = (ctx.accounts.short_margin_vault.amount as i128) - pnl_long;

        // Who's under IM?
        let (under_is_long, deficit) = if long_eq < im_required {
            (true, (im_required - long_eq) as u64)
        } else if short_eq < im_required {
            (false, (im_required - short_eq) as u64)
        } else {
            return err!(ErrorCode::NotLiquidatable);
        };

        // Compute bounty and capped take
        let bounty = bps(deficit as u128, m.liquidator_bps)? as u64;
        let take_total = deficit.saturating_add(bounty).min(max_bounty_take);

        if under_is_long {
            // from short → liquidator (bounty) and → long (deficit)
            let deficit_take = take_total.saturating_sub(bounty);
            if bounty > 0 {
                drain_to(&ctx.accounts.token_program, &ctx.accounts.short_margin_vault, &ctx.accounts.liquidator_ata, &ctx.accounts.deal_vault_auth, &d, bounty)?;
            }
            if deficit_take > 0 {
                drain_to(&ctx.accounts.token_program, &ctx.accounts.short_margin_vault, &ctx.accounts.long_margin_vault, &ctx.accounts.deal_vault_auth, &d, deficit_take)?;
            }
        } else {
            let deficit_take = take_total.saturating_sub(bounty);
            if bounty > 0 {
                drain_to(&ctx.accounts.token_program, &ctx.accounts.long_margin_vault, &ctx.accounts.liquidator_ata, &ctx.accounts.deal_vault_auth, &d, bounty)?;
            }
            if deficit_take > 0 {
                drain_to(&ctx.accounts.token_program, &ctx.accounts.long_margin_vault, &ctx.accounts.short_margin_vault, &ctx.accounts.deal_vault_auth, &d, deficit_take)?;
            }
        }

        // Refresh cached balances
        d.long_margin = ctx.accounts.long_margin_vault.amount;
        d.short_margin = ctx.accounts.short_margin_vault.amount;

        // If still under IM after attempt, pause (PoC socialized loss guard)
        let long_eq2 = (d.long_margin as i128) + pnl_long;
        let short_eq2 = (d.short_margin as i128) - pnl_long;
        if long_eq2 < im_required || short_eq2 < im_required {
            m.paused = true;
        }

        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Accounts
// ──────────────────────────────────────────────────────────────────────────────

#[account]
pub struct Market {
    pub authority: Pubkey,
    pub quote_mint: Pubkey,
    pub oracle_authority: Pubkey,
    pub stack_id: Pubkey,

    pub price_decimals: u8,
    pub quote_decimals: u8,

    pub initial_margin_bps: u16,
    pub maintenance_margin_bps: u16,
    pub fee_bps: u16,
    pub liquidator_bps: u16,
    pub price_stale_seconds: u32,

    pub last_nav: u64,
    pub last_ts: i64,

    pub paused: bool,
    pub bump: u8,

    // New risk/admin
    pub max_leverage_bps: u16,
    pub max_nav_jump_bps: u16,
    pub max_confidence_bps: u16, // 0 = disabled
    pub circuit_breaker_until: i64,
    pub mm_buffer_bps: u16,

    pub admin_threshold: u8,
    pub admins: [Pubkey; MAX_ADMINS],

    pub pending: Option<PendingParams>,
}

impl Market {
    pub const LEN: usize =
        8 + // disc
        32*4 + // keys
        1 + 1 + // decimals
        2*4 + // bps fields (im, mm, fee, liq)
        4 + // stale secs
        8 + 8 + // last_nav, last_ts
        1 + 1 + // paused, bump
        2 + 2 + 2 + 8 + 2 + // max_lev, max_jump, max_conf, breaker_until, mm_buffer
        1 + // admin_threshold
        (32*MAX_ADMINS) + // admins
        1 + PendingParams::MAX_LEN; // Option tag + pending (max)
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct PendingParams {
    pub params: MarketUpdateParams,
    pub eta: i64,
}
impl PendingParams {
    // rough upper bound for serialization (borsh)
    pub const MAX_LEN: usize = MarketUpdateParams::MAX_LEN + 8;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct MarketUpdateParams {
    pub oracle_authority: Option<Pubkey>,
    pub initial_margin_bps: Option<u16>,
    pub maintenance_margin_bps: Option<u16>,
    pub fee_bps: Option<u16>,
    pub liquidator_bps: Option<u16>,
    pub price_stale_seconds: Option<u32>,

    // new params
    pub max_leverage_bps: Option<u16>,
    pub max_nav_jump_bps: Option<u16>,
    pub max_confidence_bps: Option<u16>,
    pub mm_buffer_bps: Option<u16>,
    pub admin_threshold: Option<u8>,
}
impl MarketUpdateParams {
    pub const MAX_LEN: usize =
        (1+32) + // oracle_authority
        (1+2)*4 + // four u16 options (im, mm, fee, liq)
        (1+4) + // price_stale_seconds
        (1+2)*4 + // new u16 options
        (1+1); // admin_threshold
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct MarketInitParams {
    pub oracle_authority: Pubkey,
    pub price_decimals: u8,
    pub initial_margin_bps: u16,
    pub maintenance_margin_bps: u16,
    pub fee_bps: u16,
    pub liquidator_bps: u16,
    pub price_stale_seconds: u32,

    // new
    pub max_leverage_bps: u16,
    pub max_nav_jump_bps: u16,
    pub max_confidence_bps: Option<u16>,
    pub mm_buffer_bps: Option<u16>,
    pub admin_threshold: Option<u8>,
}

#[account]
pub struct MarketVaultAuth {
    pub market: Pubkey,
    pub bump: u8,
}
impl MarketVaultAuth {
    pub const LEN: usize = 8 + 32 + 1;
}

#[account]
pub struct Deal {
    pub market: Pubkey,
    pub long: Pubkey,
    pub short: Pubkey,

    pub size: u64,
    pub entry_nav: u64,
    pub is_open: bool,

    pub long_margin: u64,
    pub short_margin: u64,

    pub client_order_id: u64,
    pub bump: u8,
}
impl Deal {
    pub const LEN: usize = 8 + 32 + 32 + 32 + 8 + 8 + 1 + 8 + 8 + 8 + 1;
}

#[account]
pub struct DealVaultAuth {
    pub deal: Pubkey,
    pub bump: u8,
}
impl DealVaultAuth {
    pub const LEN: usize = 8 + 32 + 1;
}

// ──────────────────────────────────────────────────────────────────────────────
// Instruction Contexts
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Accounts)]
#[instruction(stack_id: Pubkey)]
pub struct InitMarket<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    pub quote_mint: Box<Account<'info, Mint>>,

    #[account(
        init,
        payer = authority,
        space = Market::LEN,
        seeds = [VERSION_SEED, b"market", authority.key().as_ref(), quote_mint.key().as_ref(), stack_id.as_ref()],
        bump
    )]
    pub market: Account<'info, Market>,

    #[account(
        init,
        payer = authority,
        space = MarketVaultAuth::LEN,
        seeds = [VERSION_SEED, b"mva", market.key().as_ref()],
        bump
    )]
    pub market_vault_auth: Account<'info, MarketVaultAuth>,

    #[account(
        init,
        payer = authority,
        associated_token::mint = quote_mint,
        associated_token::authority = market_vault_auth
    )]
    pub fee_vault: Account<'info, TokenAccount>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct AdminMarketToggle<'info> {
    pub authority: Signer<'info>,
    #[account(mut)]
    pub market: Account<'info, Market>,
}

#[derive(Accounts)]
pub struct AdminMarketParams<'info> {
    pub authority: Signer<'info>,
    #[account(mut)]
    pub market: Account<'info, Market>,
}

#[derive(Accounts)]
pub struct PostNav<'info> {
    #[account(mut)]
    pub market: Account<'info, Market>,
    pub oracle_authority: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(client_order_id: u64)]
pub struct OpenDeal<'info> {
    // parties
    #[account(mut)]
    pub long: Signer<'info>,
    #[account(mut)]
    pub short: Signer<'info>,

    // market & mint
    pub market: Account<'info, Market>,
    pub quote_mint: Box<Account<'info, Mint>>,

    // users' source ATAs
    #[account(
        mut,
        constraint = long_source.mint == quote_mint.key(),
        constraint = long_source.owner == long.key()
    )]
    pub long_source: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = short_source.mint == quote_mint.key(),
        constraint = short_source.owner == short.key()
    )]
    pub short_source: Account<'info, TokenAccount>,

    // deal state
    #[account(
        init,
        payer = long,
        space = Deal::LEN,
        seeds = [VERSION_SEED, b"deal", market.key().as_ref(), long.key().as_ref(), short.key().as_ref(), &client_order_id.to_le_bytes()],
        bump
    )]
    pub deal: Account<'info, Deal>,

    // deal vault authority
    #[account(
        init,
        payer = long,
        space = DealVaultAuth::LEN,
        seeds = [VERSION_SEED, b"deal_vault_auth", deal.key().as_ref()],
        bump
    )]
    pub deal_vault_auth: Account<'info, DealVaultAuth>,

    // margin vaults (owned by the deal_vault_auth PDA)
    #[account(
        init,
        payer = long,
        associated_token::mint = quote_mint,
        associated_token::authority = deal_vault_auth
    )]
    pub long_margin_vault: Account<'info, TokenAccount>,
    #[account(
        init,
        payer = long,
        associated_token::mint = quote_mint,
        associated_token::authority = deal_vault_auth
    )]
    pub short_margin_vault: Account<'info, TokenAccount>,

    // fee vault belongs to the market vault auth
    #[account(
        mut,
        constraint = fee_vault.mint == quote_mint.key(),
        constraint = fee_vault.owner == market_vault_auth.key(),
    )]
    pub fee_vault: Account<'info, TokenAccount>,

    pub market_vault_auth: Account<'info, MarketVaultAuth>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct AddMarginLong<'info> {
    #[account(mut)]
    pub long: Signer<'info>,
    #[account(mut, has_one = long)]
    pub deal: Account<'info, Deal>,
    pub market: Account<'info, Market>,
    pub quote_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        constraint = long_source.mint == quote_mint.key(),
        constraint = long_source.owner == long.key()
    )]
    pub long_source: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = long_margin_vault.mint == quote_mint.key(),
        constraint = long_margin_vault.owner == deal_vault_auth.key()
    )]
    pub long_margin_vault: Account<'info, TokenAccount>,

    pub deal_vault_auth: Account<'info, DealVaultAuth>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct AddMarginShort<'info> {
    #[account(mut)]
    pub short: Signer<'info>,
    #[account(mut, has_one = short)]
    pub deal: Account<'info, Deal>,
    pub market: Account<'info, Market>,
    pub quote_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        constraint = short_source.mint == quote_mint.key(),
        constraint = short_source.owner == short.key()
    )]
    pub short_source: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = short_margin_vault.mint == quote_mint.key(),
        constraint = short_margin_vault.owner == deal_vault_auth.key()
    )]
    pub short_margin_vault: Account<'info, TokenAccount>,

    pub deal_vault_auth: Account<'info, DealVaultAuth>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct CloseDeal<'info> {
    #[account(mut)]
    pub long: Signer<'info>,
    #[account(mut)]
    pub short: Signer<'info>,

    #[account(mut)]
    pub market: Account<'info, Market>,
    #[account(mut, has_one = market)]
    pub deal: Account<'info, Deal>,

    pub quote_mint: Box<Account<'info, Mint>>,

    // vaults
    #[account(
        mut,
        constraint = long_margin_vault.mint == quote_mint.key(),
        constraint = long_margin_vault.owner == deal_vault_auth.key()
    )]
    pub long_margin_vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = short_margin_vault.mint == quote_mint.key(),
        constraint = short_margin_vault.owner == deal_vault_auth.key()
    )]
    pub short_margin_vault: Account<'info, TokenAccount>,

    // payouts
    #[account(mut, constraint = long_payout_ata.mint == quote_mint.key(), constraint = long_payout_ata.owner == long.key())]
    pub long_payout_ata: Account<'info, TokenAccount>,
    #[account(mut, constraint = short_payout_ata.mint == quote_mint.key(), constraint = short_payout_ata.owner == short.key())]
    pub short_payout_ata: Account<'info, TokenAccount>,

    /// CHECK: only used as destination for close_account rent
    #[account(mut, address = market.authority)]
    pub market_authority: UncheckedAccount<'info>,

    pub deal_vault_auth: Account<'info, DealVaultAuth>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Liquidate<'info> {
    #[account(mut)]
    pub liquidator: Signer<'info>,

    #[account(mut)]
    pub market: Account<'info, Market>,
    #[account(mut, has_one = market)]
    pub deal: Account<'info, Deal>,

    pub quote_mint: Box<Account<'info, Mint>>,

    // vaults
    #[account(
        mut,
        constraint = long_margin_vault.mint == quote_mint.key(),
        constraint = long_margin_vault.owner == deal_vault_auth.key()
    )]
    pub long_margin_vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = short_margin_vault.mint == quote_mint.key(),
        constraint = short_margin_vault.owner == deal_vault_auth.key()
    )]
    pub short_margin_vault: Account<'info, TokenAccount>,

    // payouts
    #[account(mut, constraint = long_payout_ata.mint == quote_mint.key(), constraint = long_payout_ata.owner == deal.long)]
    pub long_payout_ata: Account<'info, TokenAccount>,
    #[account(mut, constraint = short_payout_ata.mint == quote_mint.key(), constraint = short_payout_ata.owner == deal.short)]
    pub short_payout_ata: Account<'info, TokenAccount>,
    #[account(mut, constraint = liquidator_ata.mint == quote_mint.key(), constraint = liquidator_ata.owner == liquidator.key())]
    pub liquidator_ata: Account<'info, TokenAccount>,

    /// CHECK: only used as destination for close_account rent
    #[account(mut, address = market.authority)]
    pub market_authority: UncheckedAccount<'info>,

    pub deal_vault_auth: Account<'info, DealVaultAuth>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct PartialLiquidate<'info> {
    #[account(mut)]
    pub liquidator: Signer<'info>,

    #[account(mut)]
    pub market: Account<'info, Market>,
    #[account(mut, has_one = market)]
    pub deal: Account<'info, Deal>,

    pub quote_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        constraint = long_margin_vault.mint == quote_mint.key(),
        constraint = long_margin_vault.owner == deal_vault_auth.key()
    )]
    pub long_margin_vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = short_margin_vault.mint == quote_mint.key(),
        constraint = short_margin_vault.owner == deal_vault_auth.key()
    )]
    pub short_margin_vault: Account<'info, TokenAccount>,

    #[account(mut, constraint = long_payout_ata.mint == quote_mint.key(), constraint = long_payout_ata.owner == deal.long)]
    pub long_payout_ata: Account<'info, TokenAccount>,
    #[account(mut, constraint = short_payout_ata.mint == quote_mint.key(), constraint = short_payout_ata.owner == deal.short)]
    pub short_payout_ata: Account<'info, TokenAccount>,
    #[account(mut, constraint = liquidator_ata.mint == quote_mint.key(), constraint = liquidator_ata.owner == liquidator.key())]
    pub liquidator_ata: Account<'info, TokenAccount>,

    /// CHECK: only used as destination for close_account rent
    #[account(mut, address = market.authority)]
    pub market_authority: UncheckedAccount<'info>,

    pub deal_vault_auth: Account<'info, DealVaultAuth>,
    pub token_program: Program<'info, Token>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Events
// ──────────────────────────────────────────────────────────────────────────────

#[event]
pub struct MarketInitialized {
    pub market: Pubkey,
    pub quote_mint: Pubkey,
    pub stack_id: Pubkey,
    pub im_bps: u16,
    pub mm_bps: u16,
    pub fee_bps: u16,
    pub liq_bps: u16,
    pub price_decimals: u8,
    pub quote_decimals: u8,
}

#[event]
pub struct NavPosted {
    pub market: Pubkey,
    pub nav: u64,
    pub ts: i64,
}

#[event]
pub struct DealOpened {
    pub deal: Pubkey,
    pub market: Pubkey,
    pub long: Pubkey,
    pub short: Pubkey,
    pub size: u64,
    pub entry_nav: u64,
    pub notional_quote: u64,
    pub long_deposit: u64,
    pub short_deposit: u64,
    pub open_fee_each: u64,
}

#[event]
pub struct DealClosed {
    pub deal: Pubkey,
    pub market: Pubkey,
    pub long_payout: u64,
    pub short_payout: u64,
    pub close_nav: u64,
}

#[event]
pub struct DealLiquidated {
    pub deal: Pubkey,
    pub market: Pubkey,
    pub bounty_paid: u64,
    pub close_nav: u64,
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers & Admin Utilities
// ──────────────────────────────────────────────────────────────────────────────

fn apply_market_updates(m: &mut Market, p: &MarketUpdateParams) {
    if let Some(x) = p.oracle_authority       { m.oracle_authority = x; }
    if let Some(x) = p.initial_margin_bps     { m.initial_margin_bps = x; }
    if let Some(x) = p.maintenance_margin_bps { m.maintenance_margin_bps = x; }
    if let Some(x) = p.fee_bps                { m.fee_bps = x; }
    if let Some(x) = p.liquidator_bps         { m.liquidator_bps = x; }
    if let Some(x) = p.price_stale_seconds    { m.price_stale_seconds = x; }

    if let Some(x) = p.max_leverage_bps       { m.max_leverage_bps = x; }
    if let Some(x) = p.max_nav_jump_bps       { m.max_nav_jump_bps = x; }
    if let Some(x) = p.max_confidence_bps     { m.max_confidence_bps = x; }
    if let Some(x) = p.mm_buffer_bps          { m.mm_buffer_bps = x; }
    if let Some(x) = p.admin_threshold        { m.admin_threshold = x; }
}

fn ensure_price_fresh(m: &Market) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    if now < m.circuit_breaker_until {
        return err!(ErrorCode::CircuitBreaker);
    }
    require!(m.last_nav > 0, ErrorCode::PriceNotSet);
    let age = now.saturating_sub(m.last_ts);
    require!(age >= 0, ErrorCode::ClockWentBackwards);
    require!((age as u64) <= m.price_stale_seconds as u64, ErrorCode::PriceStale);
    Ok(())
}

fn bps(amount: u128, bps: u16) -> Result<u128> {
    amount
        .checked_mul(bps as u128)
        .and_then(|x| x.checked_div(10_000))
        .ok_or(ErrorCode::MathOverflow.into())
}

fn ratio_bps_u128(num: u128, denom: u128) -> Result<u128> {
    num.checked_mul(10_000)
        .and_then(|x| x.checked_div(denom))
        .ok_or(ErrorCode::MathOverflow.into())
}

/// notional (quote decimals) = size(UNIT_DEC) * nav(PRICE_DEC) rescaled
fn notional_quote(
    size_units: u64,
    nav_price: u64,
    price_decimals: u8,
    quote_decimals: u8,
) -> Result<u128> {
    let prod = (size_units as u128)
        .checked_mul(nav_price as u128)
        .ok_or(ErrorCode::MathOverflow)?;
    scale_amount(prod, (UNIT_DECIMALS as u32) + (price_decimals as u32), quote_decimals as u32)
}

fn pnl_quote(
    size_units: u64,
    entry_nav: u64,
    close_nav: u64,
    price_decimals: u8,
    quote_decimals: u8,
) -> Result<i128> {
    let diff = (close_nav as i128).saturating_sub(entry_nav as i128); // signed
    let mag = (diff.unsigned_abs() as u128)
        .checked_mul(size_units as u128)
        .ok_or(ErrorCode::MathOverflow)?;
    let scaled = scale_amount(mag, (UNIT_DECIMALS as u32) + (price_decimals as u32), quote_decimals as u32)?;
    let signed = if diff >= 0 { scaled as i128 } else { -(scaled as i128) };
    Ok(signed)
}

fn scale_amount(amount: u128, from_dec: u32, to_dec: u32) -> Result<u128> {
    if from_dec == to_dec {
        return Ok(amount);
    }
    if from_dec > to_dec {
        let p = from_dec - to_dec;
        pow10_u128(p).and_then(|d| amount.checked_div(d)).ok_or(ErrorCode::MathOverflow.into())
    } else {
        let p = to_dec - from_dec;
        pow10_u128(p)
            .and_then(|m| amount.checked_mul(m))
            .ok_or(ErrorCode::MathOverflow.into())
    }
}

fn pow10_u128(p: u32) -> Option<u128> {
    Some(10u128.pow(p))
}

fn clamp_i128(x: i128, lo: i128, hi: i128) -> i128 {
    if x < lo { lo } else if x > hi { hi } else { x }
}

// CPI helpers (lifetime-safe)

fn transfer_from_user<'info>(
    token_program: &Program<'info, Token>,
    from: &Account<'info, TokenAccount>,
    to: &Account<'info, TokenAccount>,
    authority: &Signer<'info>,
    amount: u64,
) -> Result<()> {
    let cpi = CpiContext::new(
        token_program.to_account_info(),
        Transfer {
            from: from.to_account_info(),
            to: to.to_account_info(),
            authority: authority.to_account_info(),
        },
    );
    token::transfer(cpi, amount)
}

fn transfer_signed<'info>(
    token_program: &Program<'info, Token>,
    from: &Account<'info, TokenAccount>,
    to: &Account<'info, TokenAccount>,
    pda_auth: AccountInfo<'info>,
    signer_seeds: &[&[u8]],
    amount: u64,
) -> Result<()> {
    let signer_groups = [signer_seeds];
    let cpi = CpiContext::new_with_signer(
        token_program.to_account_info(),
        Transfer {
            from: from.to_account_info(),
            to: to.to_account_info(),
            authority: pda_auth,
        },
        &signer_groups,
    );
    token::transfer(cpi, amount)
}

fn drain_to<'info>(
    token_program: &Program<'info, Token>,
    from_vault: &Account<'info, TokenAccount>,
    to_account: &Account<'info, TokenAccount>,
    deal_vault_auth: &Account<'info, DealVaultAuth>,
    deal: &Account<'info, Deal>,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let deal_key = deal.key();
    let seeds: [&[u8]; 4] = [VERSION_SEED, b"deal_vault_auth", deal_key.as_ref(), &[deal_vault_auth.bump]];
    transfer_signed(
        token_program,
        from_vault,
        to_account,
        deal_vault_auth.to_account_info(),
        &seeds[..],
        amount,
    )
}

fn close_signed_token_account<'info>(
    token_program: &Program<'info, Token>,
    token_acc: &Account<'info, TokenAccount>,
    destination: &UncheckedAccount<'info>,
    deal_vault_auth: &Account<'info, DealVaultAuth>,
    deal: &Account<'info, Deal>,
) -> Result<()> {
    if token_acc.amount != 0 {
        return Ok(()); // only close when empty
    }
    let deal_key = deal.key();
    let seeds: [&[u8]; 4] = [VERSION_SEED, b"deal_vault_auth", deal_key.as_ref(), &[deal_vault_auth.bump]];
    let signer_groups = [&seeds[..]];
    let cpi = CpiContext::new_with_signer(
        token_program.to_account_info(),
        CloseAccount {
            account: token_acc.to_account_info(),
            destination: destination.to_account_info(),
            authority: deal_vault_auth.to_account_info(),
        },
        &signer_groups,
    );
    token::close_account(cpi)
}

// ──────────────────────────────────────────────────────────────────────────────
/* Admin Multisig Helpers (lifetime-decoupled) */
// ──────────────────────────────────────────────────────────────────────────────

fn require_admin_or_multisig<'a>(
    m: &Market,
    authority_key: Pubkey,
    remaining: &[AccountInfo<'a>],
) -> Result<()> {
    if authority_key == m.authority {
        return Ok(());
    }
    require_multisig(m, remaining)
}

fn require_multisig<'a>(m: &Market, infos: &[AccountInfo<'a>]) -> Result<()> {
    let mut hits: u8 = 0;
    for ai in infos {
        if !ai.is_signer { continue; }
        let k = ai.key();
        if m.admins.iter().any(|a| *a != Pubkey::default() && *a == k) {
            hits = hits.saturating_add(1);
        }
    }
    require!((hits as u8) >= m.admin_threshold, ErrorCode::NotEnoughSigners);
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Errors
// ──────────────────────────────────────────────────────────────────────────────

#[error_code]
pub enum ErrorCode {
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Market is paused")]
    MarketPaused,
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Zero size not allowed")]
    ZeroSize,
    #[msg("Price not set")]
    PriceNotSet,
    #[msg("Clock went backwards")]
    ClockWentBackwards,
    #[msg("Price is stale")]
    PriceStale,
    #[msg("Insufficient margin")]
    InsufficientMargin,
    #[msg("Deal is not open")]
    NotOpen,
    #[msg("Deal already open")]
    AlreadyOpen,
    #[msg("Not liquidatable at current NAV")]
    NotLiquidatable,

    // New error codes
    #[msg("Requested leverage exceeds limit")]
    LeverageTooHigh,
    #[msg("Oracle confidence too wide")]
    OracleConfidenceTooWide,
    #[msg("NAV jump too large; circuit breaker tripped")]
    PriceJumpTooLarge,
    #[msg("Circuit breaker active")]
    CircuitBreaker,
    #[msg("No pending params")]
    NoPendingParams,
    #[msg("Timelock not expired")]
    TimelockNotExpired,
    #[msg("Not enough admin signers")]
    NotEnoughSigners,
}

