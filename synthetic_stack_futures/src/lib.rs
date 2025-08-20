use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::AssociatedToken,
    token::{self, CloseAccount, Mint, Token, TokenAccount, Transfer},
};

declare_id!("programid");

/// PROGRAM OVERVIEW
/// - Market: defines parameters (margins, fees, oracle auth, price scale) and stores last NAV.
/// - Deal: bilateral futures position (long vs short), no underlying ever minted/held.
/// - NAV is posted by the oracle authority; all PnL is computed in quote mint (e.g., USDC).
/// - On open: both sides deposit initial margin; open fees are collected to fee_vault.
/// - On close: cash settlement using latest NAV; vaults pay out principal +/- PnL.
/// - Liquidation: allowed when either side falls below maintenance margin; liquidator gets bounty.
///
/// Fixed-point:
///   UNIT_DECIMALS = 6 (stack units), price_decimals set by market, quote_decimals from mint.

pub const UNIT_DECIMALS: u8 = 6; // "stack units" precision, e.g., 1e6
pub const VERSION_SEED: &[u8] = b"v1";

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
        market.last_nav = 0;
        market.last_ts = 0;
        market.paused = false;
        market.bump = ctx.bumps.market; // Anchor >=0.29

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
        require_keys_eq!(ctx.accounts.market.authority, ctx.accounts.authority.key(), ErrorCode::Unauthorized);
        ctx.accounts.market.paused = paused;
        Ok(())
    }

    pub fn update_market_params(ctx: Context<AdminMarketParams>, params: MarketUpdateParams) -> Result<()> {
        require_keys_eq!(ctx.accounts.market.authority, ctx.accounts.authority.key(), ErrorCode::Unauthorized);
        let m = &mut ctx.accounts.market;
        if let Some(im) = params.initial_margin_bps { m.initial_margin_bps = im; }
        if let Some(mm) = params.maintenance_margin_bps { m.maintenance_margin_bps = mm; }
        if let Some(fee) = params.fee_bps { m.fee_bps = fee; }
        if let Some(liq) = params.liquidator_bps { m.liquidator_bps = liq; }
        if let Some(ps) = params.price_stale_seconds { m.price_stale_seconds = ps; }
        if let Some(oa) = params.oracle_authority { m.oracle_authority = oa; }
        Ok(())
    }

    // Oracle posts NAV (scaled by market.price_decimals)
    pub fn post_nav(ctx: Context<PostNav>, nav: u64) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require!(!market.paused, ErrorCode::MarketPaused);
        require_keys_eq!(market.oracle_authority, ctx.accounts.oracle_authority.key(), ErrorCode::Unauthorized);

        market.last_nav = nav;
        market.last_ts = Clock::get()?.unix_timestamp;

        emit!(NavPosted {
            market: market.key(),
            nav,
            ts: market.last_ts,
        });

        Ok(())
    }

    // ──────────────────────────────────────────────────────────────────────────────
    // Trading (Bilateral Deal)
    // ──────────────────────────────────────────────────────────────────────────────

    /// Open a bilateral futures deal.
    /// - size: stack units scaled by 1e6 (UNIT_DECIMALS)
    /// - client_order_id: disambiguates multiple deals between same parties
    /// - long_deposit / short_deposit: quote-mint amounts to move into margin vaults (must >= required + fees)
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
        let deal_key = deal.key(); // avoid temporary in seeds
        let seeds: [&[u8]; 4] = [
            VERSION_SEED,
            b"deal_vault_auth",
            deal_key.as_ref(),
            &[dva.bump],
        ];
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
            notional_quote: notional_q as u64, // typical fits in u64 for tests
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
    /// Either party may call this; all funds settle to the provided payout ATAs.
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

    /// Liquidate if either side below maintenance margin at current NAV.
    /// Liquidator receives bounty from the pool before payouts.
    pub fn liquidate(ctx: Context<Liquidate>) -> Result<()> {
        let market = &ctx.accounts.market;
        let deal = &mut ctx.accounts.deal;
        require!(deal.is_open, ErrorCode::NotOpen);
        require!(!market.paused, ErrorCode::MarketPaused);
        ensure_price_fresh(market)?;

        let notional_q = notional_quote(deal.size, market.last_nav, market.price_decimals, market.quote_decimals)?;
        let mm_required = bps(notional_q, market.maintenance_margin_bps)?;

        // Compute equity for each side
        let pnl_long = pnl_quote(
            deal.size,
            deal.entry_nav,
            market.last_nav,
            market.price_decimals,
            market.quote_decimals,
        )?;
        let long_equity = (ctx.accounts.long_margin_vault.amount as i128) + pnl_long;
        let short_equity = (ctx.accounts.short_margin_vault.amount as i128) - pnl_long;

        // Must be liquidatable (either side below maintenance)
        require!(
            long_equity < mm_required as i128 || short_equity < mm_required as i128,
            ErrorCode::NotLiquidatable
        );

        // Pool & bounty
        let total_pool = (ctx.accounts.long_margin_vault.amount as u128)
            + (ctx.accounts.short_margin_vault.amount as u128);
        let bounty = bps(total_pool, market.liquidator_bps)? as u64;

        if bounty > 0 {
            // Pay bounty from LONG vault first, then SHORT
            let mut remaining = bounty;
            let long_bal = ctx.accounts.long_margin_vault.amount;
            let take_long = remaining.min(long_bal);
            if take_long > 0 {
                drain_to(
                    &ctx.accounts.token_program,
                    &ctx.accounts.long_margin_vault,
                    &ctx.accounts.liquidator_ata,
                    &ctx.accounts.deal_vault_auth,
                    &deal,
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
                    &deal,
                    remaining,
                )?;
            }
        }

        // Recompute remaining pool and do close-like payout
        let long_amt = ctx.accounts.long_margin_vault.amount as u128;
        let short_amt = ctx.accounts.short_margin_vault.amount as u128;
        let pool = long_amt + short_amt;

        let desired_long = (long_amt as i128) + pnl_long;
        let long_payout = clamp_i128(desired_long, 0, pool as i128) as u128;
        let short_payout = pool.saturating_sub(long_payout);

        if long_payout > 0 {
            drain_to(
                &ctx.accounts.token_program,
                &ctx.accounts.long_margin_vault,
                &ctx.accounts.long_payout_ata,
                &ctx.accounts.deal_vault_auth,
                &deal,
                long_payout as u64,
            )?;
        }
        if short_payout > 0 {
            drain_to(
                &ctx.accounts.token_program,
                &ctx.accounts.short_margin_vault,
                &ctx.accounts.short_payout_ata,
                &ctx.accounts.deal_vault_auth,
                &deal,
                short_payout as u64,
            )?;
        }

        // Close vaults
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

        emit!(DealLiquidated {
            deal: deal.key(),
            market: deal.market,
            bounty_paid: bounty,
            close_nav: market.last_nav,
        });

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

    pub price_decimals: u8, // e.g., 6
    pub quote_decimals: u8, // from mint

    pub initial_margin_bps: u16,
    pub maintenance_margin_bps: u16,
    pub fee_bps: u16,
    pub liquidator_bps: u16,

    pub price_stale_seconds: u32,

    pub last_nav: u64, // scaled by price_decimals
    pub last_ts: i64,

    pub paused: bool,
    pub bump: u8,
}

impl Market {
    pub const LEN: usize = 8  // disc
        + 32 + 32 + 32 + 32
        + 1 + 1
        + 2 + 2 + 2 + 2
        + 4
        + 8 + 8
        + 1 + 1;
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

    pub size: u64,       // units scaled by 1e6
    pub entry_nav: u64,  // scaled by price_decimals
    pub is_open: bool,

    pub long_margin: u64,
    pub short_margin: u64,

    pub client_order_id: u64,
    pub bump: u8,
}
impl Deal {
    pub const LEN: usize = 8  // disc
        + 32 + 32 + 32
        + 8 + 8 + 1
        + 8 + 8
        + 8 + 1;
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

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct MarketInitParams {
    pub oracle_authority: Pubkey,
    pub price_decimals: u8,       // e.g., 6
    pub initial_margin_bps: u16,  // e.g., 1000 (=10%)
    pub maintenance_margin_bps: u16, // e.g., 500 (=5%)
    pub fee_bps: u16,             // open fee (total; split half each)
    pub liquidator_bps: u16,      // bounty from pool
    pub price_stale_seconds: u32, // e.g., 300
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct MarketUpdateParams {
    pub oracle_authority: Option<Pubkey>,
    pub initial_margin_bps: Option<u16>,
    pub maintenance_margin_bps: Option<u16>,
    pub fee_bps: Option<u16>,
    pub liquidator_bps: Option<u16>,
    pub price_stale_seconds: Option<u32>,
}

#[derive(Accounts)]
#[instruction(stack_id: Pubkey)]
pub struct InitMarket <'info> {
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
        payer = long, // long pays rent; arbitrary
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
    // either party can close (kept both signers for symmetry with tests)
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

    // for closing token accounts rent refund
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
/* Helpers (lifetime-safe CPI wrappers) */
// ──────────────────────────────────────────────────────────────────────────────

fn ensure_price_fresh(market: &Market) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    require!(market.last_nav > 0, ErrorCode::PriceNotSet);
    let age = now.saturating_sub(market.last_ts);
    require!(age >= 0, ErrorCode::ClockWentBackwards);
    require!((age as u64) <= market.price_stale_seconds as u64, ErrorCode::PriceStale);
    Ok(())
}

fn bps(amount: u128, bps: u16) -> Result<u128> {
    amount
        .checked_mul(bps as u128)
        .and_then(|x| x.checked_div(10_000))
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

// Lifetime-unified helper CPIs

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
    pda_auth: AccountInfo<'info>, // pass by value to avoid ref temporaries
    signer_seeds: &[&[u8]],
    amount: u64,
) -> Result<()> {
    // Avoid temporary: wrap signer_seeds in a binding that lives through the CPI call
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
    to_ata: &Account<'info, TokenAccount>,
    deal_vault_auth: &Account<'info, DealVaultAuth>,
    deal: &Account<'info, Deal>,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let deal_key = deal.key(); // avoid temp borrow in seeds
    let seeds: [&[u8]; 4] = [
        VERSION_SEED,
        b"deal_vault_auth",
        deal_key.as_ref(),
        &[deal_vault_auth.bump],
    ];
    transfer_signed(
        token_program,
        from_vault,
        to_ata,
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
    let deal_key = deal.key(); // avoid temp borrow in seeds
    let seeds: [&[u8]; 4] = [
        VERSION_SEED,
        b"deal_vault_auth",
        deal_key.as_ref(),
        &[deal_vault_auth.bump],
    ];
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
}
