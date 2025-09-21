use crate::amm::*;
use anchor_lang::prelude::AccountMeta;
use anyhow::{Context, Result};
use jupiter_amm_interface::{
    try_get_account_data, AccountMap, AmmContext, AmmLabel, AmmProgramIdToLabel,
};
use obsidian_common::{deserialize_pool, Pool, PoolExtension};
use solana_sdk::program_pack::Pack;
use solana_sdk::sysvar;
use solana_sdk::{pubkey, pubkey::Pubkey};
use spl_token::state::Account as TokenAccount;
use std::{collections::HashMap, convert::TryInto, sync::LazyLock};

pub mod obsidian_programs {
    use super::*;
    pub const OBSIDIAN_AMM: Pubkey = pubkey!("5f14hM1CDuHPDJwz6Lb68sDT2JKmbLTutStbtGMuuFWC");
}

pub struct ObsidianAmm {
    key: Pubkey,
    label: String,
    state: Pool,
    reserve_mints: [Pubkey; 2],
    reserve_vaults: [Pubkey; 2],
    reserves: [u64; 2],
    program_id: Pubkey,
}

impl AmmProgramIdToLabel for ObsidianAmm {
    const PROGRAM_ID_TO_LABELS: &[(Pubkey, AmmLabel)] =
        &[(obsidian_programs::OBSIDIAN_AMM, "Obsidian AMM")];
}

pub static OBSIDIAN_AMM_PROGRAMS: LazyLock<HashMap<Pubkey, String>> = LazyLock::new(|| {
    HashMap::from_iter(
        ObsidianAmm::PROGRAM_ID_TO_LABELS
            .iter()
            .map(|(program_id, amm_label)| (*program_id, (*amm_label).into())),
    )
});

impl Clone for ObsidianAmm {
    fn clone(&self) -> Self {
        ObsidianAmm {
            key: self.key,
            label: self.label.clone(),
            state: self.state,
            reserve_mints: self.reserve_mints,
            reserve_vaults: self.reserve_vaults,
            program_id: self.program_id,
            reserves: self.reserves,
        }
    }
}

impl Amm for ObsidianAmm {
    fn from_keyed_account(keyed_account: &KeyedAccount, _amm_context: &AmmContext) -> Result<Self> {
        // Ensure minimum account data size for PoolV0
        let data = &keyed_account.account.data;

        // Use bytemuck for safe zero-copy deserialization
        let pool = deserialize_pool(data)
            .map_err(|e| anyhow::anyhow!("Failed to deserialize Obsidian pool: {}", e))?;

        let reserve_mints = [
            Pubkey::from(pool.token_x_mint),
            Pubkey::from(pool.token_y_mint),
        ];

        let reserve_vaults = [
            Pubkey::from(pool.token_x_vault),
            Pubkey::from(pool.token_y_vault),
        ];

        let label = OBSIDIAN_AMM_PROGRAMS
            .get(&keyed_account.account.owner)
            .cloned()
            .context("Label not found for Obsidian AMM")?;

        let program_id = keyed_account.account.owner;

        Ok(Self {
            key: keyed_account.key,
            label,
            state: pool,
            reserve_mints,
            reserve_vaults,
            program_id,
            reserves: Default::default(),
        })
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn program_id(&self) -> Pubkey {
        self.program_id
    }

    fn key(&self) -> Pubkey {
        self.key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        self.reserve_mints.to_vec()
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        vec![self.key, self.reserve_vaults[0], self.reserve_vaults[1]]
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        // Update pool state (prices, parameters may have changed)
        let data = try_get_account_data(account_map, &self.key)?;
        let pool = deserialize_pool(data)
            .map_err(|e| anyhow::anyhow!("Failed to deserialize Obsidian pool: {}", e))?;

        // Update pool state with latest data
        self.state = pool;

        // Update vault balances by unpacking token accounts
        let token_x_account_data = try_get_account_data(account_map, &self.reserve_vaults[0])?;
        let token_x_account = TokenAccount::unpack(token_x_account_data)?;

        let token_y_account_data = try_get_account_data(account_map, &self.reserve_vaults[1])?;
        let token_y_account = TokenAccount::unpack(token_y_account_data)?;

        // Update reserves with actual token account balances
        self.reserves = [token_x_account.amount.into(), token_y_account.amount.into()];

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        // Add validation
        if quote_params.amount == 0 {
            return Err(anyhow::anyhow!("Cannot quote zero amount"));
        }

        // Validate input mint is supported
        if !self.reserve_mints.contains(&quote_params.input_mint) {
            return Err(anyhow::anyhow!(
                "Unsupported input mint: {}",
                quote_params.input_mint
            ));
        }

        // Determine trade direction and use obsidian-common for quoting
        let (input_amount, output_amount) = if quote_params.input_mint == self.reserve_mints[0] {
            // X to Y (selling X tokens)
            let output = self
                .state
                .quote_x_to_y(self.reserves[0], quote_params.amount)
                .map_err(|_| anyhow::anyhow!("Obsidian quote X to Y failed"))?;
            (quote_params.amount, output)
        } else {
            // Y to X (buying X tokens with Y)
            let output = self
                .state
                .quote_y_to_x(
                    self.reserves[0],
                    self.state.mid_price_lamports,
                    quote_params.amount,
                )
                .map_err(|_| anyhow::anyhow!("Obsidian quote Y to X failed"))?;
            (quote_params.amount, output)
        };

        // Validate output amount is reasonable
        if output_amount == 0 {
            return Err(anyhow::anyhow!("Quote resulted in zero output amount"));
        }

        // Obsidian AMM has no fees
        let fee_amount = 0u64;
        let fee_pct = rust_decimal::Decimal::new(0, 0); // 0% fee

        Ok(Quote {
            fee_pct,
            in_amount: input_amount.try_into()?,
            out_amount: output_amount.try_into()?,
            fee_amount,
            fee_mint: quote_params.input_mint,
        })
    }

    fn get_accounts_len(&self) -> usize {
        7
    }

    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
        let SwapParams {
            source_mint,
            destination_token_account,
            source_token_account,
            token_transfer_authority,
            ..
        } = swap_params;

        // Validate source mint is supported
        if !self.reserve_mints.contains(source_mint) {
            return Err(anyhow::anyhow!("Unsupported source mint: {}", source_mint));
        }

        Ok(SwapAndAccountMetas {
            // Using TokenSwap as placeholder until Obsidian variant is added to jupiter-amm-interface
            swap: Swap::TokenSwap,
            account_metas: vec![
                AccountMeta::new_readonly(self.program_id, false),
                AccountMeta::new(*token_transfer_authority, true),
                AccountMeta::new(self.key, false),
                AccountMeta::new(self.reserve_vaults[0], false),
                AccountMeta::new(self.reserve_vaults[1], false),
                AccountMeta::new(*source_token_account, false),
                AccountMeta::new(*destination_token_account, false),
                AccountMeta::new_readonly(spl_token::id(), false),
                AccountMeta::new_readonly(sysvar::instructions::id(), false), // Instruction sysvar for CPI introspection
            ],
        })
    }

    fn supports_exact_out(&self) -> bool {
        false // Obsidian AMM only supports exact-in swaps
    }

    fn requires_update_for_reserve_mints(&self) -> bool {
        true // Need to update vault balances and pool state
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }
}
