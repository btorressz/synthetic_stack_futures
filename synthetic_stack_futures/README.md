# 🧮 Synthetic Stack Futures

Welcome to **Synthetic Stack Futures**! This Solana Anchor program enables fully on-chain, cash-settled, bilateral futures trading on synthetic assets. No underlying tokens are ever minted or held—everything is settled in a quote token (like USDC) using oracle-provided Net Asset Value (NAV).
**NOTE** this project is a proof of concept that was developed in solana playground ide 

## 🚀 Program Concepts & Architecture

**Synthetic Stack Futures** is designed for:

- **On-chain, bilateral futures**: Every position is a direct contract between two parties (long and short), with no third-party custody of synthetic assets.
- **Cash settlement**: All profits and losses are paid in a stable quote token (e.g., USDC), not in the synthetic asset itself.
- **Oracle-driven pricing**: The program relies on a trusted oracle authority to post the latest Net Asset Value (NAV) for each market.
- **Margin and risk management**: Initial and maintenance margin requirements, as well as fees and liquidator bounties, are enforced by the program.
- **Vault security**: All user funds are held in program-derived vaults, ensuring only the program can move funds.
- **Event-driven transparency**: All key actions emit events for easy indexing and monitoring.

### 🏦 How It Works

1. **Market Creation**: An admin creates a market, specifying parameters like margin requirements, fees, oracle authority, and price precision.
2. **NAV Posting**: The oracle authority regularly posts the latest NAV for the market, which is used for all settlement and margin calculations.
3. **Deal Opening**: Two users (long and short) open a deal by depositing margin and paying fees. The deal is uniquely identified by the parties and a client order ID.
4. **Margin Management**: Either side can add more margin at any time to avoid liquidation.
5. **Settlement**: The deal can be closed at any time by either party, settling PnL at the latest NAV. If margin falls too low, anyone can liquidate the deal and claim a bounty.

---

## 🏗️ Main Structs

### `Market`
- Stores all market parameters: authority, quote mint, oracle authority, margin/fee/bounty rates, price decimals, and NAV state.
- Handles pausing, parameter updates, and NAV posting.

**Fields:**
- `authority`: The main admin (can be rotated).
- `admins`: Up to 5 admin keys for multisig actions.
- `admin_threshold`: Number of admin signatures required for multisig actions.
- `quote_mint`: The SPL token used for margin and settlement (e.g., USDC).
- `oracle_authority`: The trusted account that posts NAV prices.
- `initial_margin_bps`, `maintenance_margin_bps`: Margin requirements in basis points.
- `fee_bps`, `liquidator_bps`: Fee and bounty rates.
- `max_leverage_bps`: Maximum leverage allowed (basis points).
- `max_nav_jump_bps`: Maximum allowed NAV jump between updates (circuit breaker).
- `max_confidence_bps`: Optional max confidence interval for oracle NAV.
- `mm_buffer_bps`: Extra buffer for maintenance margin (risk control).
- `last_nav`, `last_ts`: Latest NAV and timestamp.
- `paused`: Whether trading is paused (can be triggered by admin or risk events).
- `circuit_breaker_until`: Timestamp until which trading is paused after a risk event.
- `pending`: Optional timelocked pending parameter update.

### `Deal`
- Represents an open futures position: long/short parties, size, entry NAV, margin balances, and open/closed state.
- Each deal is uniquely identified by the parties and a client order ID.

**Fields:**
- `long`, `short`: The two counterparties.
- `size`: Position size in stack units (fixed-point, 6 decimals).
- `entry_nav`: NAV at the time of opening.
- `long_margin`, `short_margin`: Current margin balances.
- `is_open`: Whether the deal is active.

### `MarketVaultAuth` & `DealVaultAuth`
- Program-derived accounts that own the vaults for markets and deals, ensuring only the program can move funds.

---


## ⚙️ Core Instructions & Features

- **init_market**: Create a new market with custom parameters, including risk controls and multisig admin setup.
- **pause_market**: Pause or unpause trading (requires admin or multisig).
- **update_market_params**: Update market parameters (margins, fees, risk controls, etc; admin/multisig only).
- **propose_market_params**: Propose a timelocked parameter update (admin/multisig only).
- **execute_market_params**: Execute a pending parameter update after the timelock expires.
- **rotate_authority**: Change the main admin authority (admin/multisig only).
- **post_nav**: Oracle posts the latest NAV for settlement, with optional confidence interval and jump/circuit breaker checks.
- **open_deal**: Open a new bilateral futures position. Both sides deposit margin and pay fees. Leverage cap enforced at open.
- **add_margin_long/short**: Add more margin to an open deal (for long or short).
- **close_deal**: Settle the deal at the latest NAV, paying out principal and PnL.
- **liquidate**: If either side is under maintenance margin or leverage cap, anyone can liquidate and claim a bounty. Socialized loss/circuit breaker if vault depleted.
- **liquidate_to_im**: Partial liquidation to bring under-margined side back to initial margin, rewarding the liquidator but keeping the deal open if possible.

### 📝 Example Usage Flow

1. **Admin** calls `init_market` to create a new market for a synthetic asset.
2. **Oracle** regularly calls `post_nav` to update the NAV.
3. **User A** (long) and **User B** (short) agree to open a deal and both deposit margin via `open_deal`.
4. If the market moves, either party can add more margin with `add_margin_long` or `add_margin_short`.
5. When ready, either party can close the deal with `close_deal`, or if margin is too low, a third party can call `liquidate`.

---


## 🧩 Math, Risk Controls & Precision
- All calculations use fixed-point math with 6 decimals for stack units.
- Price and quote decimals are configurable per market.
- PnL and margin are always settled in the quote token (e.g., USDC).

**Key Math & Risk Controls:**
- All amounts are scaled for precision (e.g., 1.000000 stack units = 1,000,000 in code).
- PnL is calculated as: `size * (close_nav - entry_nav)` (rescaled to quote decimals).
- Margin requirements and fees are always enforced in quote token units.
- Leverage is capped at open and checked during liquidation.
- NAV updates are checked for excessive jumps (circuit breaker) and optional confidence interval.
- Socialized loss: If a vault is depleted, the market is paused to prevent cascading losses.

---


## 🛡️ Security & Admin
- All vaults are owned by program PDAs.
- Only the oracle authority can post NAV.
- Only the market authority or a multisig of admins can update parameters or pause trading.

**Security & Admin Features:**
- Multisig admin support for all sensitive actions (threshold configurable).
- Timelock for parameter changes (propose/execute flow).
- All token transfers use Anchor's CPI wrappers for safety.
- Vaults are only accessible by program PDAs, not users.
- Strict checks for authority and oracle signatures.
- All state changes emit events for transparency.
## 🆕 New Features in This Version

- Multisig admin and timelock for market parameter changes
- Circuit breaker and socialized loss floor for risk management
- Leverage and NAV jump limits
- Partial liquidation to initial margin
- Optional oracle confidence interval enforcement
- More granular error codes for all risk and admin checks

---

## 📦 Events
- `MarketInitialized`, `NavPosted`, `DealOpened`, `DealClosed`, `DealLiquidated` — for easy tracking of all key actions.

**Event Descriptions:**
- `MarketInitialized`: New market created.
- `NavPosted`: Oracle posts a new NAV.
- `DealOpened`: A new deal is opened between two parties.
- `DealClosed`: A deal is settled and closed.
- `DealLiquidated`: A deal is forcibly closed due to insufficient margin.

---


## 🧑‍💻 Error Codes
- Custom errors for math overflow, unauthorized actions, stale prices, insufficient margin, excessive leverage, circuit breaker, timelock, and more.

**Common Errors:**
- `MathOverflow`: Calculation overflowed.
- `MarketPaused`: Trading is paused.
- `Unauthorized`: Action not allowed by this signer.
- `ZeroSize`: Deal size must be positive.
- `PriceNotSet`: NAV not yet posted.
- `PriceStale`: NAV is too old for settlement.
- `InsufficientMargin`: Not enough margin to open or maintain a deal.
- `NotOpen` / `AlreadyOpen`: Deal state errors.
- `NotLiquidatable`: Deal cannot be liquidated at current NAV.
- `LeverageTooHigh`: Requested leverage exceeds market cap.
- `OracleConfidenceTooWide`: Oracle confidence interval too wide.
- `PriceJumpTooLarge`: NAV jump too large; circuit breaker tripped.
- `CircuitBreaker`: Circuit breaker is active.
- `NoPendingParams`: No pending parameter update to execute.
- `TimelockNotExpired`: Timelock for parameter update not expired.
- `NotEnoughSigners`: Not enough admin signers for multisig action.

---

## 📚 Example Scenario

> **Alice** and **Bob** want to trade a synthetic future on the "Stack Index". Alice is bullish (long), Bob is bearish (short).
>
> 1. The admin creates a market for Stack Index, setting margin and fee parameters.
> 2. The oracle posts the current NAV: 100.000000.
> 3. Alice and Bob open a deal for 10 stack units. Both deposit margin and pay fees.
> 4. The NAV rises to 110.000000. Alice is in profit, Bob is at risk.
> 5. If Bob's margin falls below maintenance, anyone can liquidate the deal and claim a bounty.
> 6. Otherwise, Alice and Bob can close the deal and receive their payouts.

---

## 🛠️ Technical Details

- **Language:** Rust (Anchor framework)
- **Precision:** 6 decimals for stack units, configurable for price and quote tokens
- **Vaults:** SPL Token accounts owned by program PDAs
- **Oracles:** Any trusted account can be set as the oracle authority(will add pyth in future)
- **Fees:** Collected to a market fee vault, withdrawable by the market authority

---
