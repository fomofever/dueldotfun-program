use anchor_lang::prelude::*;
use anchor_lang::system_program;

#[cfg(not(feature = "no-entrypoint"))]
use solana_security_txt::security_txt;

declare_id!("FUQ2JFRFU1s23U4FdZPotAaFrJ1nNH3R9bhWrxUnVBMb");

#[cfg(not(feature = "no-entrypoint"))]
security_txt! {
    name: "duel.fun",
    project_url: "https://duel.fun",
    contacts: "link:https://github.com/fomofever/dueldotfun/security",
    policy: "We do not currently pay a bug bounty. Please report any vulnerabilities responsibly via GitHub Security Advisories.",
    source_code: "https://github.com/fomofever/dueldotfun-program",
    preferred_languages: "en",
    auditors: "None"
}

// ── Constants ─────────────────────────────────────────────────────────
// Platform fee varies by timeframe — longer duels get lower fees (higher rewards for winners)
// 5m→5%  15m→4%  1h→3%  4h→2%  24h→1%
pub const BPS_DENOMINATOR: u64 = 10_000;

fn fee_bps_for_timeframe(timeframe: &str) -> u64 {
    match timeframe {
        "5m"  => 500,  // 5.00%
        "15m" => 400,  // 4.00%
        "1h"  => 300,  // 3.00%
        "4h"  => 200,  // 2.00%
        "24h" => 100,  // 1.00%
        _     => 500,  // default fallback
    }
}
pub const MIN_BET_LAMPORTS:   u64 = 100_000_000; // 0.1 SOL
pub const DRAW_THRESHOLD_BPS: u64 = 50;          // 0.5%

/// Receives platform fees and all unclaimed pool funds when a pool is closed.
/// Separate from the admin/resolver key.
pub const TREASURY_PUBKEY: &str = "DyLadoLEenwtVgknW1Zi69sdXRthZZtCyWDR6LayJZKT";

/// Signs pool-resolution and pool-close transactions.
/// Separate from the fee treasury so the resolver cannot skim fees.
pub const ADMIN_PUBKEY: &str = "4d8ja7kBZBeSSNnWTjzyDDTuuoMSDKDbEKFPbeYYoM4D";

// PDA seeds
pub const POOL_SEED:   &[u8] = b"bet_pool";
pub const ESCROW_SEED: &[u8] = b"escrow";

#[program]
pub mod duel {
    use super::*;

    /// Create a new duel pool.
    pub fn create_pool(
        ctx: Context<CreatePool>,
        token_mint:    Pubkey,
        timeframe:     String,
        start_slot:    u64,
        start_price:   u64,   // price * 1e9 (fixed point)
        end_timestamp: i64,
    ) -> Result<()> {
        require!(timeframe.len() <= 4, DuelError::InvalidTimeframe);
        require!(end_timestamp > Clock::get()?.unix_timestamp, DuelError::InvalidEndTime);
        require!(start_price > 0, DuelError::InvalidStartPrice);

        let pool = &mut ctx.accounts.pool;
        pool.authority     = ctx.accounts.authority.key();
        pool.treasury      = treasury_pubkey()?;
        pool.escrow        = ctx.accounts.escrow.key();
        pool.token_mint    = token_mint;
        pool.timeframe     = timeframe;
        pool.start_slot    = start_slot;
        pool.start_price   = start_price;
        pool.end_timestamp = end_timestamp;
        pool.total_pump    = 0;
        pool.total_rug     = 0;
        pool.outcome       = Outcome::Pending;
        pool.bump          = ctx.bumps.pool;

        let escrow = &mut ctx.accounts.escrow;
        escrow.pool            = pool.key();
        escrow.bump            = ctx.bumps.escrow;
        escrow.total_deposited = 0;
        escrow.total_paid_out  = 0;
        escrow.fee_collected   = 0;
        escrow.resolved        = false;

        emit!(PoolCreated {
            pool:          pool.key(),
            token_mint,
            timeframe:     pool.timeframe.clone(),
            end_timestamp,
        });

        Ok(())
    }

    /// Place a bet — transfers SOL into the pool escrow.
    /// One bet per wallet per pool. Attempting a second bet from the same wallet
    /// will fail because the bet PDA already exists.
    pub fn place_bet(
        ctx: Context<PlaceBet>,
        side:     Side,
        lamports: u64,
    ) -> Result<()> {
        require!(lamports >= MIN_BET_LAMPORTS, DuelError::BetTooSmall);
        require!(ctx.accounts.pool.authority != Pubkey::default(), DuelError::InvalidAuthority);

        let pool = &ctx.accounts.pool;
        require!(pool.outcome == Outcome::Pending, DuelError::PoolResolved);
        require!(
            Clock::get()?.unix_timestamp < pool.end_timestamp,
            DuelError::PoolExpired
        );
        require_keys_eq!(ctx.accounts.escrow.key(), pool.escrow, DuelError::InvalidEscrow);

        // Transfer SOL into escrow
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                system_program::Transfer {
                    from: ctx.accounts.bettor.to_account_info(),
                    to:   ctx.accounts.escrow.to_account_info(),
                },
            ),
            lamports,
        )?;

        // Record bet
        let bet = &mut ctx.accounts.bet;
        bet.pool     = pool.key();
        bet.bettor   = ctx.accounts.bettor.key();
        bet.side     = side;
        bet.lamports = lamports;
        bet.claimed  = false;
        bet.bump     = ctx.bumps.bet;

        let escrow = &mut ctx.accounts.escrow;
        escrow.total_deposited = escrow.total_deposited
            .checked_add(lamports)
            .ok_or(DuelError::Overflow)?;

        // Update pool totals
        let pool = &mut ctx.accounts.pool;
        match side {
            Side::Pump => pool.total_pump = pool.total_pump.checked_add(lamports).ok_or(DuelError::Overflow)?,
            Side::Rug  => pool.total_rug  = pool.total_rug.checked_add(lamports).ok_or(DuelError::Overflow)?,
        }

        emit!(BetPlaced {
            pool:     pool.key(),
            bettor:   ctx.accounts.bettor.key(),
            side,
            lamports,
        });

        Ok(())
    }

    /// Resolve a pool after timeframe expires.
    /// Callable by the pool creator OR the designated admin.
    /// Transfers the platform fee to the treasury immediately.
    pub fn resolve_pool(
        ctx: Context<ResolvePool>,
        end_price: u64,   // price * 1e9 (fixed point)
    ) -> Result<()> {
        let pool = &ctx.accounts.pool;
        let caller_key = ctx.accounts.authority.key();

        require!(
            caller_key == pool.authority || caller_key == admin_pubkey()?,
            DuelError::InvalidAuthority
        );
        require!(pool.outcome == Outcome::Pending, DuelError::PoolResolved);
        require!(
            Clock::get()?.unix_timestamp >= pool.end_timestamp,
            DuelError::PoolNotExpired
        );
        require!(end_price > 0, DuelError::InvalidEndPrice);
        require_keys_eq!(ctx.accounts.escrow.key(), pool.escrow, DuelError::InvalidEscrow);
        require_keys_eq!(ctx.accounts.treasury.key(), pool.treasury, DuelError::InvalidTreasury);

        // Guard against double-resolution
        {
            let escrow_state = &ctx.accounts.escrow;
            require!(!escrow_state.resolved, DuelError::PoolResolved);
        }

        let outcome = determine_outcome(pool.start_price, end_price);

        let pool = &mut ctx.accounts.pool;
        pool.outcome   = outcome;
        pool.end_price = end_price;

        // Calculate and transfer platform fee to treasury
        let total   = pool.total_pump.checked_add(pool.total_rug).ok_or(DuelError::Overflow)?;
        let fee_bps = fee_bps_for_timeframe(&pool.timeframe);
        let fee     = total
            .checked_mul(fee_bps).ok_or(DuelError::Overflow)?
            .checked_div(BPS_DENOMINATOR).ok_or(DuelError::Overflow)?;

        if fee > 0 {
            **ctx.accounts.escrow.to_account_info().try_borrow_mut_lamports()? =
                ctx.accounts.escrow.to_account_info().lamports()
                    .checked_sub(fee).ok_or(DuelError::Overflow)?;
            **ctx.accounts.treasury.try_borrow_mut_lamports()? =
                ctx.accounts.treasury.lamports()
                    .checked_add(fee).ok_or(DuelError::Overflow)?;
        }

        {
            let escrow_state = &mut ctx.accounts.escrow;
            escrow_state.fee_collected = fee;
            escrow_state.resolved = true;
        }

        emit!(PoolResolved {
            pool:      pool.key(),
            outcome,
            end_price,
            total_sol: total,
            fee_sol:   fee,
        });

        Ok(())
    }

    /// Claim winnings for a winning bet (pull model — each winner calls this themselves).
    /// The bet PDA is closed and the rent is returned to the bettor.
    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        let pool = &ctx.accounts.pool;
        let bet  = &ctx.accounts.bet;

        require!(pool.outcome != Outcome::Pending, DuelError::PoolNotResolved);
        require!(!bet.claimed, DuelError::AlreadyClaimed);
        require_keys_eq!(ctx.accounts.escrow.key(), pool.escrow, DuelError::InvalidEscrow);

        let total   = pool.total_pump.checked_add(pool.total_rug).ok_or(DuelError::Overflow)?;
        let fee_bps = fee_bps_for_timeframe(&pool.timeframe);
        let net_bps = BPS_DENOMINATOR.checked_sub(fee_bps).ok_or(DuelError::Overflow)?;
        let net     = total
            .checked_mul(net_bps).ok_or(DuelError::Overflow)?
            .checked_div(BPS_DENOMINATOR).ok_or(DuelError::Overflow)?;

        let payout: u64 = match pool.outcome {
            Outcome::Draw => {
                // Pro-rata refund of net pool (fee already taken at resolution)
                bet.lamports
                    .checked_mul(net_bps).ok_or(DuelError::Overflow)?
                    .checked_div(BPS_DENOMINATOR).ok_or(DuelError::Overflow)?
            }
            Outcome::Pump if bet.side == Side::Pump => {
                if pool.total_pump == 0 { 0 }
                else {
                    net.checked_mul(bet.lamports).ok_or(DuelError::Overflow)?
                       .checked_div(pool.total_pump).ok_or(DuelError::Overflow)?
                }
            }
            Outcome::Rug if bet.side == Side::Rug => {
                if pool.total_rug == 0 { 0 }
                else {
                    net.checked_mul(bet.lamports).ok_or(DuelError::Overflow)?
                       .checked_div(pool.total_rug).ok_or(DuelError::Overflow)?
                }
            }
            _ => 0, // Lost
        };

        require!(payout > 0, DuelError::NoPayout);

        // Sanity-check accounting
        let remaining = {
            let e = &ctx.accounts.escrow;
            e.total_deposited
                .checked_sub(e.fee_collected).ok_or(DuelError::Overflow)?
                .checked_sub(e.total_paid_out).ok_or(DuelError::Overflow)?
        };
        require!(remaining >= payout, DuelError::InsufficientEscrowBalance);

        // Transfer payout from escrow to bettor
        **ctx.accounts.escrow.to_account_info().try_borrow_mut_lamports()? =
            ctx.accounts.escrow.to_account_info().lamports()
                .checked_sub(payout).ok_or(DuelError::Overflow)?;
        **ctx.accounts.bettor.try_borrow_mut_lamports()? =
            ctx.accounts.bettor.lamports()
                .checked_add(payout).ok_or(DuelError::Overflow)?;

        {
            let e = &mut ctx.accounts.escrow;
            e.total_paid_out = e.total_paid_out.checked_add(payout).ok_or(DuelError::Overflow)?;
        }

        // Mark bet as claimed before the account is closed by Anchor
        let bet = &mut ctx.accounts.bet;
        bet.claimed = true;
        bet.payout  = payout;

        emit!(Claimed {
            pool:   ctx.accounts.pool.key(),
            bettor: ctx.accounts.bettor.key(),
            payout,
        });

        // Anchor closes the bet PDA at the end of this instruction,
        // returning the rent lamports to the bettor.
        Ok(())
    }

    /// Close a losing (or refunded-draw) bet PDA and return the rent to the bettor.
    /// Winners must use `claim` instead. This is purely a rent-reclaim instruction
    /// for bettors whose side lost.
    pub fn close_bet(ctx: Context<CloseBet>) -> Result<()> {
        let pool = &ctx.accounts.pool;
        let bet  = &ctx.accounts.bet;

        require!(pool.outcome != Outcome::Pending, DuelError::PoolNotResolved);
        require!(!bet.claimed, DuelError::AlreadyClaimed);

        // If the bettor would receive a payout they must use `claim` instead.
        let has_payout = match pool.outcome {
            Outcome::Pump    => bet.side == Side::Pump,
            Outcome::Rug     => bet.side == Side::Rug,
            Outcome::Draw    => true,   // draws always get a refund via claim
            Outcome::Pending => false,
        };
        require!(!has_payout, DuelError::UseClaimInstead);

        emit!(BetClosed {
            pool:   ctx.accounts.pool.key(),
            bettor: ctx.accounts.bettor.key(),
        });

        // Anchor closes the bet PDA and returns rent to the bettor.
        Ok(())
    }

    /// Close a resolved pool — sweeps any unclaimed funds from the escrow to the
    /// treasury, then closes both the pool and escrow accounts.
    ///
    /// Callable by the pool creator OR the admin. Typically called by the admin
    /// after all (or most) winners have claimed, or after a reasonable waiting
    /// period has elapsed.
    ///
    /// Unclaimed winnings revert to the treasury — document this in your T&C.
    pub fn close_pool(ctx: Context<ClosePool>) -> Result<()> {
        let pool = &ctx.accounts.pool;
        let caller_key = ctx.accounts.authority.key();

        require!(
            caller_key == pool.authority || caller_key == admin_pubkey()?,
            DuelError::InvalidAuthority
        );
        require!(pool.outcome != Outcome::Pending, DuelError::PoolNotResolved);
        require_keys_eq!(ctx.accounts.escrow.key(), pool.escrow, DuelError::InvalidEscrow);

        emit!(PoolClosed {
            pool: ctx.accounts.pool.key(),
        });

        // Anchor closes:
        //   • pool account   → rent returned to authority
        //   • escrow account → all remaining lamports (unclaimed winnings + rent)
        //                      sent to treasury
        Ok(())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

fn treasury_pubkey() -> Result<Pubkey> {
    Pubkey::try_from(TREASURY_PUBKEY).map_err(|_| error!(DuelError::InvalidTreasury))
}

fn admin_pubkey() -> Result<Pubkey> {
    Pubkey::try_from(ADMIN_PUBKEY).map_err(|_| error!(DuelError::InvalidAuthority))
}

fn determine_outcome(start_price: u64, end_price: u64) -> Outcome {
    if start_price == 0 { return Outcome::Draw; }

    // Use u128 for intermediate multiplication to prevent u64 overflow for
    // high-priced tokens (end_price - start_price) * BPS_DENOMINATOR can
    // exceed u64::MAX. saturating_sub is used to satisfy the arithmetic linter
    // even though the if/else branches already guarantee no underflow.
    // checked_sub is guarded by the if/else but we use it explicitly for the linter.
    // u128 multiplication by BPS_DENOMINATOR (10_000) can never overflow u128 in practice.
    let change_bps: u64 = if end_price >= start_price {
        let diff = end_price.checked_sub(start_price).unwrap_or(0) as u128;
        diff.checked_mul(BPS_DENOMINATOR as u128)
            .and_then(|v| v.checked_div(start_price as u128))
            .map(|v| if v > u64::MAX as u128 { u64::MAX } else { v as u64 })
            .unwrap_or(u64::MAX)
    } else {
        let diff = start_price.checked_sub(end_price).unwrap_or(0) as u128;
        diff.checked_mul(BPS_DENOMINATOR as u128)
            .and_then(|v| v.checked_div(start_price as u128))
            .map(|v| if v > u64::MAX as u128 { u64::MAX } else { v as u64 })
            .unwrap_or(u64::MAX)
    };

    if change_bps <= DRAW_THRESHOLD_BPS {
        Outcome::Draw
    } else if end_price >= start_price {
        Outcome::Pump
    } else {
        Outcome::Rug
    }
}

// ── Account structs ───────────────────────────────────────────────────

#[account]
#[derive(Default, InitSpace)]
pub struct Pool {
    pub authority:     Pubkey,    // 32
    pub treasury:      Pubkey,    // 32
    pub escrow:        Pubkey,    // 32
    pub token_mint:    Pubkey,    // 32
    #[max_len(4)]
    pub timeframe:     String,    // 4 (len prefix) + 4 (max content) = 8
    pub start_slot:    u64,       // 8
    pub start_price:   u64,       // 8
    pub end_price:     u64,       // 8
    pub end_timestamp: i64,       // 8
    pub total_pump:    u64,       // 8
    pub total_rug:     u64,       // 8
    pub outcome:       Outcome,   // 1
    pub bump:          u8,        // 1
}

impl Pool {
    // 8 disc | 4×32 pubkeys | 4+4 timeframe | 6×8 numerics | 1 outcome | 1 bump | 64 padding
    // = 8 + 128 + 8 + 48 + 2 + 64 = 258
    pub const LEN: usize = 258;
}

#[account]
#[derive(Default, InitSpace)]
pub struct EscrowVault {
    pub pool:            Pubkey, // 32
    pub bump:            u8,     // 1
    pub total_deposited: u64,    // 8
    pub total_paid_out:  u64,    // 8
    pub fee_collected:   u64,    // 8
    pub resolved:        bool,   // 1
}

impl EscrowVault {
    // 8 disc | 32 pool | 1 bump | 3×8 u64s | 1 bool = 66
    pub const LEN: usize = 66;
}

#[account]
#[derive(Default, InitSpace)]
pub struct Bet {
    pub pool:     Pubkey,  // 32
    pub bettor:   Pubkey,  // 32
    pub side:     Side,    // 1
    pub lamports: u64,     // 8
    pub payout:   u64,     // 8
    pub claimed:  bool,    // 1
    pub bump:     u8,      // 1
}

impl Bet {
    // 8 disc | 2×32 pubkeys | 1 side | 2×8 u64s | 1 bool | 1 bump = 91
    pub const LEN: usize = 91;
}

// ── Enums ─────────────────────────────────────────────────────────────

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Default, InitSpace)]
pub enum Outcome {
    #[default]
    Pending,
    Pump,
    Rug,
    Draw,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Default, InitSpace)]
pub enum Side {
    #[default]
    Pump,
    Rug,
}

// ── Contexts ──────────────────────────────────────────────────────────

#[derive(Accounts)]
#[instruction(token_mint: Pubkey, timeframe: String, start_slot: u64)]
pub struct CreatePool<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(
        init,
        payer  = authority,
        space  = Pool::LEN,
        seeds  = [POOL_SEED, token_mint.as_ref(), timeframe.as_bytes(), &start_slot.to_le_bytes()],
        bump,
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        init,
        payer = authority,
        space = EscrowVault::LEN,
        seeds = [ESCROW_SEED, pool.key().as_ref()],
        bump,
    )]
    pub escrow: Account<'info, EscrowVault>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct PlaceBet<'info> {
    #[account(mut)]
    pub bettor: Signer<'info>,

    #[account(mut)]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [ESCROW_SEED, pool.key().as_ref()],
        bump  = escrow.bump,
    )]
    pub escrow: Account<'info, EscrowVault>,

    #[account(
        init,
        payer  = bettor,
        space  = Bet::LEN,
        seeds  = [b"bet", pool.key().as_ref(), bettor.key().as_ref()],
        bump,
    )]
    pub bet: Account<'info, Bet>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ResolvePool<'info> {
    #[account(mut)]
    pub pool: Account<'info, Pool>,

    pub authority: Signer<'info>,

    #[account(
        mut,
        has_one = pool,
        seeds   = [ESCROW_SEED, pool.key().as_ref()],
        bump    = escrow.bump,
    )]
    pub escrow: Account<'info, EscrowVault>,

    /// CHECK: verified against pool.treasury
    #[account(mut, address = pool.treasury @ DuelError::InvalidTreasury)]
    pub treasury: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    #[account(mut)]
    pub bettor: Signer<'info>,

    pub pool: Account<'info, Pool>,

    /// The bet PDA is closed at the end of this instruction.
    /// The rent (≈0.0017 SOL) is returned to the bettor automatically.
    #[account(
        mut,
        close  = bettor,
        has_one = bettor,
        has_one = pool,
        seeds  = [b"bet", pool.key().as_ref(), bettor.key().as_ref()],
        bump   = bet.bump,
    )]
    pub bet: Account<'info, Bet>,

    #[account(
        mut,
        has_one = pool,
        seeds   = [ESCROW_SEED, pool.key().as_ref()],
        bump    = escrow.bump,
    )]
    pub escrow: Account<'info, EscrowVault>,

    pub system_program: Program<'info, System>,
}

/// Close a losing bet PDA and reclaim rent. Only valid for bets that lost;
/// winners (including draw refunds) must use `claim`.
#[derive(Accounts)]
pub struct CloseBet<'info> {
    #[account(mut)]
    pub bettor: Signer<'info>,

    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        close   = bettor,
        has_one = bettor,
        has_one = pool,
        seeds   = [b"bet", pool.key().as_ref(), bettor.key().as_ref()],
        bump    = bet.bump,
    )]
    pub bet: Account<'info, Bet>,

    pub system_program: Program<'info, System>,
}

/// Close a resolved pool. The pool account rent goes to the caller;
/// all remaining escrow lamports (unclaimed winnings + escrow rent) go to the treasury.
#[derive(Accounts)]
pub struct ClosePool<'info> {
    #[account(
        mut,
        close = authority,
    )]
    pub pool: Account<'info, Pool>,

    pub authority: Signer<'info>,

    #[account(
        mut,
        has_one = pool,
        close   = treasury,
        seeds   = [ESCROW_SEED, pool.key().as_ref()],
        bump    = escrow.bump,
    )]
    pub escrow: Account<'info, EscrowVault>,

    /// CHECK: verified against pool.treasury
    #[account(mut, address = pool.treasury @ DuelError::InvalidTreasury)]
    pub treasury: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

// ── Events ────────────────────────────────────────────────────────────

#[event]
pub struct PoolCreated {
    pub pool:          Pubkey,
    pub token_mint:    Pubkey,
    pub timeframe:     String,
    pub end_timestamp: i64,
}

#[event]
pub struct BetPlaced {
    pub pool:     Pubkey,
    pub bettor:   Pubkey,
    pub side:     Side,
    pub lamports: u64,
}

#[event]
pub struct PoolResolved {
    pub pool:      Pubkey,
    pub outcome:   Outcome,
    pub end_price: u64,
    pub total_sol: u64,
    pub fee_sol:   u64,
}

#[event]
pub struct Claimed {
    pub pool:   Pubkey,
    pub bettor: Pubkey,
    pub payout: u64,
}

#[event]
pub struct BetClosed {
    pub pool:   Pubkey,
    pub bettor: Pubkey,
}

#[event]
pub struct PoolClosed {
    pub pool: Pubkey,
}

// ── Errors ────────────────────────────────────────────────────────────

#[error_code]
pub enum DuelError {
    #[msg("Bet amount below minimum (0.1 SOL)")]
    BetTooSmall,
    #[msg("Pool is already resolved")]
    PoolResolved,
    #[msg("Pool is not yet resolved")]
    PoolNotResolved,
    #[msg("Pool timeframe has not expired yet")]
    PoolNotExpired,
    #[msg("Pool timeframe has expired")]
    PoolExpired,
    #[msg("Invalid timeframe string")]
    InvalidTimeframe,
    #[msg("End time must be in the future")]
    InvalidEndTime,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("Bet already claimed")]
    AlreadyClaimed,
    #[msg("No payout — bet lost or invalid")]
    NoPayout,
    #[msg("Invalid start price")]
    InvalidStartPrice,
    #[msg("Invalid end price")]
    InvalidEndPrice,
    #[msg("Invalid treasury account")]
    InvalidTreasury,
    #[msg("Invalid authority")]
    InvalidAuthority,
    #[msg("Invalid escrow account")]
    InvalidEscrow,
    #[msg("Escrow does not have enough remaining balance for payout")]
    InsufficientEscrowBalance,
    #[msg("This bet has a payout — use the claim instruction instead of close_bet")]
    UseClaimInstead,
}
