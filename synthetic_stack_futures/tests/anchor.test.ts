// Globals available in Playground: web3, anchor, pg, BN, assert
//Minmial test file(still reviewing)


describe("Synthetic Stack Futures – minimal test (no SPL helpers)", () => {
  it("initMarket + postNav + pause toggle", async () => {
    const wallet = pg.wallet; // has publicKey & signTransaction

    // --- constants for SPL Token Program (classic token; not token-2022) ---
    const TOKEN_PROGRAM_ID = new web3.PublicKey(
      "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
    );
    const ASSOCIATED_TOKEN_PROGRAM_ID = new web3.PublicKey(
      "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
    );
    // Size of a Mint account (classic token = 82 bytes)
    const MINT_SIZE = 82;

    // Helper: build InitializeMint instruction data (tag = 0)
    // layout: u8(tag=0) | u8(decimals) | [32]mintAuthority | u8(hasFreezeAuth) | [32]?freezeAuthority
    function createInitializeMintIx(
      mintPubkey,
      decimals,
      mintAuthority,
      freezeAuthority
    ) {
      const tag = 0; // InitializeMint
      const hasFreeze = freezeAuthority ? 1 : 0;

      const data = Buffer.alloc(1 + 1 + 32 + 1 + (hasFreeze ? 32 : 0));
      let o = 0;
      data.writeUInt8(tag, o); o += 1;
      data.writeUInt8(decimals, o); o += 1;
      Buffer.from(mintAuthority.toBuffer()).copy(data, o); o += 32;
      data.writeUInt8(hasFreeze, o); o += 1;
      if (hasFreeze) {
        Buffer.from(freezeAuthority.toBuffer()).copy(data, o); o += 32;
      }

      return new web3.TransactionInstruction({
        programId: TOKEN_PROGRAM_ID,
        keys: [
          { pubkey: mintPubkey, isSigner: false, isWritable: true },
          { pubkey: web3.SYSVAR_RENT_PUBKEY, isSigner: false, isWritable: false },
        ],
        data,
      });
    }

    // --- (optional) airdrop for fees on local validator ---
    try {
      const sig = await pg.connection.requestAirdrop(wallet.publicKey, 1_000_000_000);
      await pg.connection.confirmTransaction(sig, "confirmed");
      console.log("✅ Airdrop successful");
    } catch (e) {
      console.log("⚠️ Airdrop failed (probably already funded):", e.message);
    }

    // --- 1) Create a quote mint (6 decimals) WITHOUT using spl helpers ---
    console.log("=== Creating Quote Mint ===");
    const decimals = 6;
    const mintKp = web3.Keypair.generate();
    const mintRent = await pg.connection.getMinimumBalanceForRentExemption(MINT_SIZE);

    const createMintAccIx = web3.SystemProgram.createAccount({
      fromPubkey: wallet.publicKey,
      newAccountPubkey: mintKp.publicKey,
      lamports: mintRent,
      space: MINT_SIZE,
      programId: TOKEN_PROGRAM_ID,
    });

    const initMintIx = createInitializeMintIx(
      mintKp.publicKey,
      decimals,
      wallet.publicKey, // mint authority
      null              // no freeze authority
    );

    // Send tx to create + init mint
    {
      const tx = new web3.Transaction().add(createMintAccIx, initMintIx);
      tx.feePayer = wallet.publicKey;
      tx.recentBlockhash = (await pg.connection.getLatestBlockhash()).blockhash;
      tx.partialSign(mintKp);
      const signed = await wallet.signTransaction(tx);
      const sig = await pg.connection.sendRawTransaction(signed.serialize(), { skipPreflight: false });
      await pg.connection.confirmTransaction(sig, "confirmed");
      console.log("✅ Quote mint created:", mintKp.publicKey.toBase58());
    }

    // --- 2) Derive PDAs (match seeds in your lib.rs) ---
    console.log("=== Deriving PDAs ===");
    // Seeds:
    // - market: [b"v1", b"market", authority, quote_mint, stack_id]
    // - mva:    [b"v1", b"mva", market]
    const PROGRAM_ID = pg.program.programId;
    const VERSION_SEED = Buffer.from("v1");
    const stackId = web3.Keypair.generate().publicKey;

    const [marketPda] = await web3.PublicKey.findProgramAddress(
      [
        VERSION_SEED,
        Buffer.from("market"),
        wallet.publicKey.toBuffer(),
        mintKp.publicKey.toBuffer(),
        stackId.toBuffer(),
      ],
      PROGRAM_ID
    );

    const [mvaPda] = await web3.PublicKey.findProgramAddress(
      [VERSION_SEED, Buffer.from("mva"), marketPda.toBuffer()],
      PROGRAM_ID
    );

    // Correctly derive the ATA for the fee vault (owned by mvaPda)
    const [feeVaultAta] = await web3.PublicKey.findProgramAddress(
      [
        mvaPda.toBuffer(),                    // owner (MVA PDA)
        TOKEN_PROGRAM_ID.toBuffer(),          // token program
        mintKp.publicKey.toBuffer(),          // mint
      ],
      ASSOCIATED_TOKEN_PROGRAM_ID
    );

    console.log("Market PDA:", marketPda.toBase58());
    console.log("MVA PDA:", mvaPda.toBase58());
    console.log("Fee Vault ATA:", feeVaultAta.toBase58());
    console.log("Stack ID:", stackId.toBase58());

    // --- 3) Init params (small/sane for PoC) ---
    const params = {
      oracleAuthority: wallet.publicKey,
      priceDecimals: 6,
      initialMarginBps: 1000,       // 10%
      maintenanceMarginBps: 500,    // 5%
      feeBps: 10,                   // 0.10% total
      liquidatorBps: 50,            // 0.50% bounty
      priceStaleSeconds: 300,       // 5 min
      maxLeverageBps: 10_000,       // 10x
      maxNavJumpBps: 5_000,         // 50%
      maxConfidenceBps: 0,          // disable confidence gate
      mmBufferBps: 100,             // +1% buffer
      adminThreshold: 1,            // single-sig admin
    };

    // --- 4) Debug accounts before calling initMarket ---
    console.log("=== Account Debugging ===");
    console.log("Authority:", wallet.publicKey.toBase58());
    console.log("Quote Mint:", mintKp.publicKey.toBase58());
    console.log("Program ID:", PROGRAM_ID.toBase58());

    // Check if required accounts exist
    const accounts = [
      { name: "Authority", pubkey: wallet.publicKey },
      { name: "Quote Mint", pubkey: mintKp.publicKey },
      { name: "System Program", pubkey: web3.SystemProgram.programId },
      { name: "Token Program", pubkey: TOKEN_PROGRAM_ID },
      { name: "Associated Token Program", pubkey: ASSOCIATED_TOKEN_PROGRAM_ID },
      { name: "Rent Sysvar", pubkey: web3.SYSVAR_RENT_PUBKEY },
    ];

    for (const acc of accounts) {
      try {
        const info = await pg.connection.getAccountInfo(acc.pubkey);
        console.log(`${acc.name}: ${info ? "EXISTS" : "MISSING"}`);
        if (acc.name === "Quote Mint" && info) {
          console.log(`  Owner: ${info.owner.toBase58()}`);
          console.log(`  Lamports: ${info.lamports}`);
        }
      } catch (e) {
        console.log(`${acc.name}: ERROR -`, e.message);
      }
    }

    // Check if PDAs already exist (they shouldn't before init)
    const marketInfo = await pg.connection.getAccountInfo(marketPda);
    const mvaInfo = await pg.connection.getAccountInfo(mvaPda);
    const feeVaultInfo = await pg.connection.getAccountInfo(feeVaultAta);

    console.log("Market PDA exists:", !!marketInfo);
    console.log("MVA PDA exists:", !!mvaInfo);
    console.log("Fee Vault ATA exists:", !!feeVaultInfo);
    console.log("=========================");

    // --- 5) Call initMarket ---
    console.log("=== Calling initMarket ===");
    try {
      const txInit = await pg.program.methods
        .initMarket(stackId, params)
        .accounts({
          authority: wallet.publicKey,
          quoteMint: mintKp.publicKey,
          market: marketPda,
          marketVaultAuth: mvaPda,
          feeVault: feeVaultAta,
          systemProgram: web3.SystemProgram.programId,
          tokenProgram: TOKEN_PROGRAM_ID,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          rent: web3.SYSVAR_RENT_PUBKEY,
        })
        .rpc();
      
      await pg.connection.confirmTransaction(txInit, "confirmed");
      console.log("✅ initMarket transaction confirmed:", txInit);
      
    } catch (error) {
      console.error("❌ initMarket failed:", error);
      
      // Get transaction logs for more details
      if (error.logs) {
        console.log("Transaction logs:");
        error.logs.forEach((log, i) => {
          console.log(`  ${i}: ${log}`);
        });
      }
      
      // Try to get more details about the error
      if (error.message) {
        console.log("Error message:", error.message);
      }
      
      throw error;
    }

    // Fetch and sanity check market
    console.log("=== Verifying Market Creation ===");
    const marketAcc = await pg.program.account.market.fetch(marketPda);
    console.log("Market authority:", marketAcc.authority.toBase58());
    console.log("Market quoteMint:", marketAcc.quoteMint.toBase58());
    console.log("Market priceDecimals:", marketAcc.priceDecimals);
    console.log("Market quoteDecimals:", marketAcc.quoteDecimals);

    assert.equal(marketAcc.authority.toBase58(), wallet.publicKey.toBase58());
    assert.equal(marketAcc.quoteMint.toBase58(), mintKp.publicKey.toBase58());
    assert.equal(marketAcc.priceDecimals, 6);
    assert.equal(marketAcc.quoteDecimals, 6);
    console.log("✅ Market validation passed");

    // --- 6) Post NAV (no confidence => null) ---
    console.log("=== Posting NAV ===");
    const nav = new BN(1_234_567);
    const txNav = await pg.program.methods
      .postNav(nav, null)
      .accounts({
        market: marketPda,
        oracleAuthority: wallet.publicKey,
      })
      .rpc();
    await pg.connection.confirmTransaction(txNav, "confirmed");

    const marketAfter = await pg.program.account.market.fetch(marketPda);
    console.log("NAV posted:", marketAfter.lastNav.toString());
    assert.equal(marketAfter.lastNav.toString(), nav.toString());
    console.log("✅ NAV posting successful");

    // --- 7) Pause / Unpause ---
    console.log("=== Testing Pause/Unpause ===");
    
    // Pause
    const txPause = await pg.program.methods
      .pauseMarket(true)
      .accounts({
        authority: wallet.publicKey,
        market: marketPda,
      })
      .rpc();
    await pg.connection.confirmTransaction(txPause, "confirmed");
    
    const paused = await pg.program.account.market.fetch(marketPda);
    console.log("Market paused:", paused.paused);
    assert.equal(paused.paused, true);

    // Unpause
    const txUnpause = await pg.program.methods
      .pauseMarket(false)
      .accounts({
        authority: wallet.publicKey,
        market: marketPda,
      })
      .rpc();
    await pg.connection.confirmTransaction(txUnpause, "confirmed");
    
    const unpaused = await pg.program.account.market.fetch(marketPda);
    console.log("Market unpaused:", !unpaused.paused);
    assert.equal(unpaused.paused, false);
    console.log("✅ Pause/Unpause successful");

    console.log("🎉 All tests passed - minimal flow OK!");
  });
});

//Test Output
/*

  Synthetic Stack Futures – minimal test (no SPL helpers)
    ✅ Airdrop successful
    === Creating Quote Mint ===
    ✅ Quote mint created: 9ws6P59qPwkZuiew3Y6RYN39SygnaM2KPnDT1b5bcbmn
    === Deriving PDAs ===
    Market PDA: A9jXHZQeB3t7iHemCGcSPFrjMnv8gBvr52mvzCenGcQn
    MVA PDA: 3m1ocEaFYFWW1dZLsJKSfG6gPaPZw5XHg29aJtXhs4JJ
    Fee Vault ATA: 2TvBmwLeWvbbbopU9upSRggMdjfxCwGnKMsmChyL8zcV
    Stack ID: 5ND8kYvf3sbCVeLAPnB9gd2WSoKLbXkMCfMpfPisAwqs
    === Account Debugging ===
    Authority: AqM9yiMxJGdxLsk4kQTde6sNR6i1nF8MRKwzetYiUdR7
    Quote Mint: 9ws6P59qPwkZuiew3Y6RYN39SygnaM2KPnDT1b5bcbmn
    Program ID: FSBdeh58ourJm9Wjf1BFZ8jSGrgbhN2jrF3Vw4BdiQx1
    Authority: EXISTS
    Quote Mint: EXISTS
      Owner: TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
      Lamports: 1461600
    System Program: EXISTS
    Token Program: EXISTS
    Associated Token Program: EXISTS
    Rent Sysvar: EXISTS
    Market PDA exists: false
    MVA PDA exists: false
    Fee Vault ATA exists: false
    =========================
    === Calling initMarket ===
    ✅ initMarket transaction confirmed: 2XLgGLDmQKJvg5LwE1rSw9PSMefS3j4BiMsvQGTDv9stCMAJozdejRz44z2X4jEoHWZPPF8rLS2k6nBFcaUHUm9j
    === Verifying Market Creation ===
    Market authority: AqM9yiMxJGdxLsk4kQTde6sNR6i1nF8MRKwzetYiUdR7
    Market quoteMint: 9ws6P59qPwkZuiew3Y6RYN39SygnaM2KPnDT1b5bcbmn
    Market priceDecimals: 6
    Market quoteDecimals: 6
    ✅ Market validation passed
    === Posting NAV ===
    NAV posted: 1234567
    ✅ NAV posting successful
    === Testing Pause/Unpause ===
    Market paused: true
    Market unpaused: true
    ✅ Pause/Unpause successful
    🎉 All tests passed - minimal flow OK!
    ✔ initMarket + postNav + pause toggle (7973ms)
  1 passing (8s)
$ 







*/
