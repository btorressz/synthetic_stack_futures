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
