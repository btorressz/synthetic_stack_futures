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
