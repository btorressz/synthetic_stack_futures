# synthetic_stack_futures

# 🧬 Synthetic Stack Futures

Synthetic Stack Futures is a Solana program (built with the Anchor framework) that enables **cash-settled, bilateral synthetic futures** using SPL tokens. Market parameters, margin requirements, and NAV settlement are all managed on-chain. The protocol is designed for transparency, flexibility, and direct P2P (long vs short) trading with no underlying asset ever minted or held.

**NOTE**: This project is a Proof of concept


---

## 📝 Overview

- **Markets** define margin, fee, oracle authority, and price parameters.
- **Deal** is a bilateral contract between a long and a short party.
- **NAV** (Net Asset Value) is posted by an oracle; all settlement and PnL are in the quote SPL token (e.g., USDC).
- **Cash Settlement**: No synthetic or underlying assets are minted or held; all settlement occurs in the quote token.
- **Liquidation**: If margin falls below required levels, a third-party liquidator can trigger forced settlement and receive a bounty.
- **Governance**: Admin updates require a two-step, timelocked process with optional N-of-M multisig approval.
 - **Risk Controls**: Built-in leverage caps, NAV sanity checks, maintenance buffer, partial liquidation, and a socialized loss guard.

---


## 🏗️ Program Architecture

### 🗃️ Core Structures

- **Market**: Stores market configuration, authority, margin/fee parameters, oracle, and the most recent NAV.
- **Deal**: Represents an open bilateral futures position (long vs short), including margin vaults, entry NAV, size, and state.
- **MarketVaultAuth** & **DealVaultAuth**: Program Derived Addresses (PDAs) acting as authorities for market and deal vaults, respectively.

### 🔄 Instruction Flows

1. **Market Lifecycle**
   - `init_market`: Create a new market with specified parameters.
   - `pause_market` / `update_market_params`: Admin controls for pausing and updating market configuration.
   - `post_nav`: Oracle posts NAV (price); required for settlement.

2. **Deal Lifecycle**
   - `open_deal`: Two parties (long/short) open a new position, deposit initial margin, pay fees.
   - `add_margin_long` / `add_margin_short`: Either side can add extra margin.
   - `close_deal`: Either party can close and settle at the latest NAV (cash payout).
   - `liquidate`: Third party can forcibly close if either side falls below maintenance margin, earning a bounty.

---


## 💡 Key Concepts

- **Margin**: Both parties must deposit initial margin; if it drops below maintenance margin, liquidation is possible.
- **Fee Vault**: Market collects open fees on each deal, sent to a special vault.
- **Cash Settlement**: All trades settle in the quote SPL token (no synthetic tokens).
- **NAV Oracle**: A trusted authority posts NAV, driving PnL and settlement.

---

## 🏦 Accounts & Structures

### 📈 Market

| Field                      | Description                                              |
|----------------------------|----------------------------------------------------------|
| authority                  | Market admin                                             |
| quote_mint                 | SPL token mint used for settlement (e.g., USDC)          |
| oracle_authority           | Only address allowed to post NAV                         |
| stack_id                   | Unique market ID                                         |
| price_decimals             | Decimal precision for NAV                                |
| quote_decimals             | Decimal precision for quote SPL token                    |
| initial_margin_bps         | Initial margin requirement (basis points)                |
| maintenance_margin_bps     | Maintenance margin requirement (basis points)            |
| fee_bps                    | Fee (bps) on notional, split equally                    |
| liquidator_bps             | Liquidation bounty (bps)                                 |
| price_stale_seconds        | NAV expiration threshold                                 |
| last_nav                   | Last posted NAV                                          |
| last_ts                    | Timestamp of last NAV post                               |
| paused                     | Market paused flag                                       |
| bump                       | PDA bump                                                 |

### 🤝 Deal

| Field             | Description                                      |
|-------------------|--------------------------------------------------|
| market            | Associated market                                |
| long/short        | Parties to the deal                              |
| size              | Stack units, scaled by 1e6                       |
| entry_nav         | NAV at entry (price)                             |
| is_open           | Deal open/closed flag                            |
| long_margin       | Margin in quote tokens for long                  |
| short_margin      | Margin in quote tokens for short                 |
| client_order_id   | Used to disambiguate between multiple deals      |
| bump              | PDA bump                                         |

### 📦 Other Structs

- **MarketInitParams**: Used for market initialization.
- **MarketUpdateParams**: Used to update market parameters.
- **VaultAuth**: PDA for vault authority.

---

## 🛠️ Instructions

### 🏛️ Market Management

- **init_market**: Create a new market with parameters (margin, fees, oracle, etc.).
- **pause_market**: Pause/unpause trading.
- **update_market_params**: Update margin, fee, oracle, or price staleness parameters.
- **post_nav**: Oracle posts the current NAV (price).

### ⚡ Trading

- **open_deal**: Two parties (long/short) open a deal, deposit margin, pay fees.
- **add_margin_long / add_margin_short**: Add funds to margin vaults.
- **close_deal**: Settle the deal at the current NAV and distribute payouts.
- **liquidate**: If margin is insufficient, anyone can force-close the deal and claim a bounty.

---

## 🎉 Events

| Event             | Description                                                        |
|-------------------|--------------------------------------------------------------------|
| MarketInitialized | New market created                                                 |
| NavPosted         | NAV posted by oracle                                               |
| DealOpened        | New deal opened                                                    |
| DealClosed        | Deal cash-settled and closed                                       |
| DealLiquidated    | Deal forcibly closed by liquidator                                 |

---

## 🚨 Error Codes

| Code                | Message                                   |
|---------------------|-------------------------------------------|
| MathOverflow        | Math overflow                             |
| MarketPaused        | Market is paused                          |
| Unauthorized        | Unauthorized                              |
| ZeroSize            | Zero size not allowed                     |
| PriceNotSet         | Price not set                             |
| ClockWentBackwards  | Clock went backwards                      |
| PriceStale          | Price is stale                            |
| InsufficientMargin  | Insufficient margin                       |
| NotOpen             | Deal is not open                          |
| AlreadyOpen         | Deal already open                         |
| NotLiquidatable     | Not liquidatable at current NAV           |

---

## 🧮 Math & Precision

- **Fixed-Point Units**: All stack units use 6 decimals (`UNIT_DECIMALS = 6`).
- **Price Decimals**: Set per-market for NAV precision.
- **Quote Decimals**: Pulled from SPL token mint.
- **All margin, fees, and PnL are calculated in quote token units and use basis points for margin/fees.**

---

## 🛡️ Security Considerations

- **Oracles**: Only the `oracle_authority` can post NAV; ensure this is a trusted, secure account.
- **Admin Controls**: Only market `authority` can pause or update parameters.
- **Safe Token Handling**: All token transfers and vault management use Anchor SPL wrappers and PDAs.
- **Liquidation**: The protocol is designed to allow anyone to liquidate under-collateralized positions for a bounty, incentivizing protocol health.

---

## 📄 License

MIT LICENSE
