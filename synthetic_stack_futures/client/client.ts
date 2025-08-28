// client.ts — Playground client for Synthetic Stack Futures (matches your lib.rs)
// Globals available in Playground: web3, pg, BN, assert

// Basic info
console.log("My address:", pg.wallet.publicKey.toString());
const balance = await pg.connection.getBalance(pg.wallet.publicKey);
console.log(`My balance: ${balance / web3.LAMPORTS_PER_SOL} SOL`);

// Playground dynamic handles
const PG = pg;
const PROGRAM = PG && PG.program ? PG.program : null;
const PROGRAM_ID = PROGRAM ? PROGRAM.programId : new web3.PublicKey("11111111111111111111111111111111");
const WALLET = PG && PG.wallet ? PG.wallet : null;
const CONNECTION = PG && PG.connection ? PG.connection : null;

if (!PROGRAM) console.warn("pg.program not found — build & deploy the program in Playground first.");

// Constants (match lib.rs)
const TOKEN_PROGRAM_ID = new web3.PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ASSOCIATED_TOKEN_PROGRAM_ID = new web3.PublicKey("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const SYSVAR_RENT = web3.SYSVAR_RENT_PUBKEY;

const VERSION_SEED = Buffer.from("v1");
const MARKET_SEED = Buffer.from("market");
const MVA_SEED = Buffer.from("mva");

// ---------------- Utilities ----------------
function toPubkey(x) {
  if (x instanceof web3.PublicKey) return x;
  return new web3.PublicKey(x);
}

// derive market PDA: seeds = [v1, "market", authority, quote_mint, stack_id]
async function deriveMarketPda(authorityPubkey, quoteMintPubkey, stackIdPubkey) {
  const seeds = [VERSION_SEED, MARKET_SEED, authorityPubkey.toBuffer(), quoteMintPubkey.toBuffer(), stackIdPubkey.toBuffer()];
  const [marketPda, bump] = await web3.PublicKey.findProgramAddress(seeds, PROGRAM_ID);
  return { marketPda, bump };
}

// derive mva PDA: seeds = [v1, "mva", market]
async function deriveMvaPda(marketPda) {
  const seeds = [VERSION_SEED, MVA_SEED, marketPda.toBuffer()];
  const [mvaPda, bump] = await web3.PublicKey.findProgramAddress(seeds, PROGRAM_ID);
  return { mvaPda, bump };
}

// derive deal PDA: seeds = [v1, "deal", market, long, short, client_order_id_le_bytes(8)]
async function deriveDealPda(marketPda, longPubkey, shortPubkey, clientOrderId) {
  // clientOrderId as 8-byte little-endian buffer
  const bn = new BN(clientOrderId.toString());
  const clientBuf = Buffer.from(bn.toArray("le", 8)); // 8 bytes LE
  const seeds = [VERSION_SEED, Buffer.from("deal"), marketPda.toBuffer(), longPubkey.toBuffer(), shortPubkey.toBuffer(), clientBuf];
  const [dealPda, bump] = await web3.PublicKey.findProgramAddress(seeds, PROGRAM_ID);
  return { dealPda, bump };
}

// derive ATA for an owner (PDA or Pubkey): associated token seeds = [owner, token_program_id, mint], program = associated token program
function deriveAtaForOwner(ownerPubkey, mintPubkey) {
  return web3.PublicKey.findProgramAddressSync(
    [ownerPubkey.toBuffer(), TOKEN_PROGRAM_ID.toBuffer(), mintPubkey.toBuffer()],
    ASSOCIATED_TOKEN_PROGRAM_ID
  )[0];
}

// ---------------- High-level instruction helpers ----------------

// initMarket(stackId: Pubkey, paramsObj)
async function initMarket(stackIdPubkey, paramsObj) {
  if (!PROGRAM) throw new Error("PROGRAM missing; deploy program first.");
  const authority = WALLET.publicKey;
  const quoteMint = toPubkey(paramsObj.quoteMint);

  const { marketPda } = await deriveMarketPda(authority, quoteMint, stackIdPubkey);
  const { mvaPda } = await deriveMvaPda(marketPda);
  const feeVault = deriveAtaForOwner(mvaPda, quoteMint);

  console.log("initMarket -> market:", marketPda.toBase58());
  console.log("initMarket -> mva:", mvaPda.toBase58());
  console.log("initMarket -> feeVault (ATA):", feeVault.toBase58());

  const tx = await PROGRAM.methods
    .initMarket(stackIdPubkey, paramsObj)
    .accounts({
      authority: authority,
      quoteMint: quoteMint,
      market: marketPda,
      marketVaultAuth: mvaPda,
      feeVault: feeVault,
      systemProgram: web3.SystemProgram.programId,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      rent: SYSVAR_RENT,
    })
    .rpc();
  console.log("initMarket tx:", tx);
  return { tx, marketPda, mvaPda, feeVault };
}

// postNav(market, nav (u64), confidence (Option<u64>))
async function postNav(marketPda, nav, confidence = null) {
  if (!PROGRAM) throw new Error("PROGRAM missing.");
  const navBn = new BN(nav.toString());
  const confBn = confidence !== null && confidence !== undefined ? new BN(confidence.toString()) : null;
  const tx = await PROGRAM.methods
    .postNav(navBn, confBn)
    .accounts({
      market: marketPda,
      oracleAuthority: WALLET.publicKey,
    })
    .rpc();
  console.log("postNav tx:", tx);
  return tx;
}

// pauseMarket(market, paused)
async function pauseMarket(marketPda, paused) {
  if (!PROGRAM) throw new Error("PROGRAM missing.");
  const tx = await PROGRAM.methods
    .pauseMarket(paused)
    .accounts({
      authority: WALLET.publicKey,
      market: marketPda,
    })
    .rpc();
  console.log("pauseMarket tx:", tx);
  return tx;
}

// propose_market_params(market, paramsObj, delay_secs)
async function proposeMarketParams(marketPda, paramsObj, delaySecs) {
  if (!PROGRAM) throw new Error("PROGRAM missing.");
  const tx = await PROGRAM.methods
    .proposeMarketParams(paramsObj, delaySecs)
    .accounts({
      authority: WALLET.publicKey,
      market: marketPda,
    })
    .rpc();
  console.log("proposeMarketParams tx:", tx);
  return tx;
}

// execute_market_params(market)
async function executeMarketParams(marketPda) {
  if (!PROGRAM) throw new Error("PROGRAM missing.");
  const tx = await PROGRAM.methods
    .executeMarketParams()
    .accounts({
      authority: WALLET.publicKey,
      market: marketPda,
    })
    .rpc();
  console.log("executeMarketParams tx:", tx);
  return tx;
}

// rotate_authority(market, new_authority_pubkey)
async function rotateAuthority(marketPda, newAuthorityPubkey) {
  if (!PROGRAM) throw new Error("PROGRAM missing.");
  const tx = await PROGRAM.methods
    .rotateAuthority(newAuthorityPubkey)
    .accounts({
      authority: WALLET.publicKey,
      market: marketPda,
    })
    .rpc();
  console.log("rotateAuthority tx:", tx);
  return tx;
}

// openDeal(opts) where opts includes: marketPda, quoteMint, long, short, longSourceAta, shortSourceAta, clientOrderId, size, longDeposit, shortDeposit
async function openDeal(opts) {
  if (!PROGRAM) throw new Error("PROGRAM missing.");
  const marketPda = toPubkey(opts.marketPda);
  const quoteMint = toPubkey(opts.quoteMint);
  const long = toPubkey(opts.long);
  const short = toPubkey(opts.short);
  const longSource = toPubkey(opts.longSourceAta);
  const shortSource = toPubkey(opts.shortSourceAta);
  const clientOrderId = opts.clientOrderId ?? 0;
  const size = new BN(opts.size.toString());
  const longDeposit = new BN(opts.longDeposit.toString());
  const shortDeposit = new BN(opts.shortDeposit.toString());

  const { dealPda } = await deriveDealPda(marketPda, long, short, clientOrderId);
  const { mvaPda } = await deriveMvaPda(marketPda);
  const feeVault = deriveAtaForOwner(mvaPda, quoteMint);

  console.log("openDeal -> market:", marketPda.toBase58());
  console.log("openDeal -> deal (derived):", dealPda.toBase58());
  console.log("openDeal -> feeVault:", feeVault.toBase58());

  // Call program; Anchor will create PDAs and ATAs as specified by lib.rs (payer = long for many inits)
  const tx = await PROGRAM.methods
    .openDeal(new BN(clientOrderId.toString()), size, longDeposit, shortDeposit)
    .accounts({
      long: long,
      short: short,
      market: marketPda,
      quoteMint: quoteMint,
      longSource: longSource,
      shortSource: shortSource,
      deal: dealPda,
      // For init accounts (deal_vault_auth, long_margin_vault, short_margin_vault) Anchor expects the client to
      // pass the addresses. In practice you should derive them and pass here; leaving null will fail at runtime.
      dealVaultAuth: null,
      longMarginVault: null,
      shortMarginVault: null,
      feeVault: feeVault,
      marketVaultAuth: mvaPda,
      systemProgram: web3.SystemProgram.programId,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      rent: SYSVAR_RENT,
    })
    .rpc();

  console.log("openDeal tx:", tx);
  return { tx, dealPda, feeVault, mvaPda };
}

// add_margin_long(deal, longSourceAta, longMarginVault, market)
async function addMarginLong(dealPda, longSourceAta, longMarginVault, marketPda, amount) {
  if (!PROGRAM) throw new Error("PROGRAM missing.");
  const amt = new BN(amount.toString());
  const tx = await PROGRAM.methods
    .addMarginLong(amt)
    .accounts({
      long: WALLET.publicKey,
      deal: dealPda,
      market: marketPda,
      quoteMint: null, // fill if required by your lib.rs context
      longSource: longSourceAta,
      longMarginVault: longMarginVault,
      dealVaultAuth: null, // fill with derived deal_vault_auth if needed
      tokenProgram: TOKEN_PROGRAM_ID,
    })
    .rpc();
  console.log("addMarginLong tx:", tx);
  return tx;
}

// add_margin_short(...)
async function addMarginShort(dealPda, shortSourceAta, shortMarginVault, marketPda, amount) {
  if (!PROGRAM) throw new Error("PROGRAM missing.");
  const amt = new BN(amount.toString());
  const tx = await PROGRAM.methods
    .addMarginShort(amt)
    .accounts({
      short: WALLET.publicKey,
      deal: dealPda,
      market: marketPda,
      quoteMint: null,
      shortSource: shortSourceAta,
      shortMarginVault: shortMarginVault,
      dealVaultAuth: null,
      tokenProgram: TOKEN_PROGRAM_ID,
    })
    .rpc();
  console.log("addMarginShort tx:", tx);
  return tx;
}

// closeDeal(accountsObj) — provide full accounts object matching lib.rs CloseDeal context
async function closeDeal(accountsObj) {
  if (!PROGRAM) throw new Error("PROGRAM missing.");
  const tx = await PROGRAM.methods
    .closeDeal()
    .accounts(accountsObj)
    .rpc();
  console.log("closeDeal tx:", tx);
  return tx;
}

// liquidate(accountsObj) — provide full accounts matching Liquidate context
async function liquidate(accountsObj) {
  if (!PROGRAM) throw new Error("PROGRAM missing.");
  const tx = await PROGRAM.methods
    .liquidate()
    .accounts(accountsObj)
    .rpc();
  console.log("liquidate tx:", tx);
  return tx;
}

// liquidateToIm(accountsObj, max_bounty_take)
async function liquidateToIm(accountsObj, maxBountyTake) {
  if (!PROGRAM) throw new Error("PROGRAM missing.");
  const tx = await PROGRAM.methods
    .liquidateToIm(new BN(maxBountyTake.toString()))
    .accounts(accountsObj)
    .rpc();
  console.log("liquidateToIm tx:", tx);
  return tx;
}

// ---------------- Inspectors ----------------
async function whoAmI() {
  if (!WALLET || !CONNECTION) {
    console.warn("whoAmI: missing wallet or connection");
    return null;
  }
  console.log("Wallet:", WALLET.publicKey.toBase58());
  const bal = await CONNECTION.getBalance(WALLET.publicKey);
  console.log("Balance (SOL):", bal / web3.LAMPORTS_PER_SOL);
  return { wallet: WALLET.publicKey, balance: bal };
}

async function inspectMarket(authority, quoteMint, stackId) {
  if (!CONNECTION) throw new Error("Connection missing");
  const { marketPda } = await deriveMarketPda(toPubkey(authority), toPubkey(quoteMint), toPubkey(stackId));
  const { mvaPda } = await deriveMvaPda(marketPda);
  const feeVault = deriveAtaForOwner(mvaPda, toPubkey(quoteMint));

  const marketInfo = await CONNECTION.getAccountInfo(marketPda);
  const mvaInfo = await CONNECTION.getAccountInfo(mvaPda);
  const feeInfo = await CONNECTION.getAccountInfo(feeVault);

  console.log("Market PDA:", marketPda.toBase58(), "exists:", !!marketInfo);
  console.log("MVA PDA  :", mvaPda.toBase58(), "exists:", !!mvaInfo);
  console.log("Fee ATA  :", feeVault.toBase58(), "exists:", !!feeInfo);

  if (marketInfo && PROGRAM) {
    const marketAcc = await PROGRAM.account.market.fetch(marketPda);
    console.log("market.authority:", marketAcc.authority.toBase58());
    console.log("market.quoteMint:", marketAcc.quoteMint.toBase58 ? marketAcc.quoteMint.toBase58() : marketAcc.quoteMint);
    console.log("market.priceDecimals:", marketAcc.priceDecimals);
    console.log("market.quoteDecimals:", marketAcc.quoteDecimals);
  }
  return { marketPda, mvaPda, feeVault, marketInfo, mvaInfo, feeInfo };
}

/* ---------------- Example usage (comment/uncomment to run) ----------------
(async () => {
  await whoAmI();

  // create stackId
  const stackId = web3.Keypair.generate().publicKey;

  // replace with a valid quote mint pubkey you created or existing
  const quoteMint = new web3.PublicKey("<YOUR_QUOTE_MINT_PUBKEY>");

  const params = {
    quoteMint: quoteMint,
    oracleAuthority: WALLET.publicKey,
    priceDecimals: 6,
    initial_margin_bps: 1000,
    maintenance_margin_bps: 500,
    fee_bps: 10,
    liquidator_bps: 50,
    price_stale_seconds: 300,
    max_leverage_bps: 10000,
    max_nav_jump_bps: 5000,
    max_confidence_bps: 0,
    mm_buffer_bps: 100,
    admin_threshold: 1,
  };

  const res = await initMarket(stackId, params);
  console.log(res);

  // post NAV
  // await postNav(res.marketPda, 1234567, null);

  // pause/unpause
  // await pauseMarket(res.marketPda, true);
  // await pauseMarket(res.marketPda, false);
})();
---------------------------------------------------------------------------- */

