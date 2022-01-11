use super::pool::{Child as PoolChild, ChildMut as PoolChildMut};
use super::{Address, Amount, Balance, Coin, Decimal, Give, Pool, Symbol};
use crate::abci::{BeginBlock, EndBlock};
use crate::call::Call;
use crate::client::Client;
use crate::collections::{Entry, EntryMap, Map};
use crate::context::GetContext;
use crate::encoding::{Decode, Encode};
use crate::plugins::{BeginBlockCtx, EndBlockCtx, Paid, Signer, Validators};
use crate::query::Query;
use crate::state::State;
use crate::store::Store;
use crate::{Error, Result};
use sha2::{Digest, Sha256};
use std::convert::TryInto;

mod delegator;
pub use delegator::*;

mod validator;
pub use validator::*;

#[cfg(test)]
const UNBONDING_SECONDS: u64 = 10; // 10 seconds
#[cfg(not(test))]
const UNBONDING_SECONDS: u64 = 60 * 60 * 24 * 7 * 2; // 2 weeks
const MAX_OFFLINE_BLOCKS: u64 = 100;
const MAX_VALIDATORS: u64 = 100;

#[derive(Call, Query, Client)]
pub struct Staking<S: Symbol> {
    validators: Pool<Address, Validator<S>, S>,
    amount_delegated: Amount,
    consensus_keys: Map<Address, Address>,
    last_signed_block: Map<[u8; 20], u64>,
    max_validators: u64,
    validators_by_power: EntryMap<ValidatorPowerEntry>,
    last_indexed_power: Map<Address, u64>,
    last_validator_powers: Map<Address, u64>,
}

#[derive(Entry)]
struct ValidatorPowerEntry {
    #[key]
    inverted_power: u64,
    #[key]
    address_bytes: [u8; 32],
}

impl ValidatorPowerEntry {
    fn power(&self) -> u64 {
        u64::max_value() - self.inverted_power
    }
}

impl<S: Symbol> EndBlock for Staking<S> {
    fn end_block(&mut self, _ctx: &EndBlockCtx) -> Result<()> {
        self.end_block_step()
    }
}

impl<S: Symbol> State for Staking<S> {
    type Encoding = StakingEncoding<S>;

    fn create(store: Store, data: Self::Encoding) -> Result<Self> {
        Ok(Self {
            validators: State::create(store.sub(&[0]), data.validators)?,
            amount_delegated: State::create(store.sub(&[1]), data.amount_delegated)?,
            consensus_keys: State::create(store.sub(&[2]), ())?,
            last_signed_block: State::create(store.sub(&[3]), ())?,
            validators_by_power: State::create(store.sub(&[4]), ())?,
            last_validator_powers: State::create(store.sub(&[5]), ())?,
            max_validators: State::create(store.sub(&[6]), data.max_validators)?,
            last_indexed_power: State::create(store.sub(&[7]), ())?,
        })
    }

    fn flush(self) -> Result<Self::Encoding> {
        self.consensus_keys.flush()?;
        self.last_signed_block.flush()?;
        Ok(Self::Encoding {
            max_validators: self.max_validators,
            validators: self.validators.flush()?,
            amount_delegated: self.amount_delegated.flush()?,
        })
    }
}

impl<S: Symbol> From<Staking<S>> for StakingEncoding<S> {
    fn from(staking: Staking<S>) -> Self {
        Self {
            max_validators: staking.max_validators,
            validators: staking.validators.into(),
            amount_delegated: staking.amount_delegated.into(),
        }
    }
}

impl<S: Symbol> BeginBlock for Staking<S> {
    fn begin_block(&mut self, ctx: &BeginBlockCtx) -> Result<()> {
        if let Some(last_commit_info) = &ctx.last_commit_info {
            let height = ctx.height;
            // Update last online height
            last_commit_info
                .votes
                .iter()
                .filter(|vote_info| vote_info.signed_last_block)
                .filter(|vote_info| vote_info.validator.is_some())
                .map(|vote_info| vote_info.validator.as_ref().unwrap())
                .try_for_each(|validator| {
                    self.last_signed_block.insert(
                        validator.address[..]
                            .try_into()
                            .expect("Invalid pub key hash length"),
                        height,
                    )
                })?;

            let mut offline_validator_hashes: Vec<Vec<u8>> = vec![];
            self.last_signed_block
                .iter()?
                .try_for_each(|res| -> Result<()> {
                    let (hash, last_height) = res?;
                    if *last_height + MAX_OFFLINE_BLOCKS < height {
                        offline_validator_hashes.push(hash.to_vec());
                    }

                    Ok(())
                })?;

            for hash in offline_validator_hashes.iter() {
                let val_addresses = self.val_address_for_consensus_key_hash(hash.clone())?;
                for address in val_addresses {
                    if self.slashable_balance(address)? > 0 {
                        self.slash(address, 0)?.burn();
                    }
                    let key: [u8; 20] = hash
                        .clone()
                        .try_into()
                        .map_err(|_e| Error::Coins("Invalid pubkey hash length".into()))?;
                    self.last_signed_block.remove(key)?;
                }
            }
        }

        for evidence in &ctx.byzantine_validators {
            match &evidence.validator {
                Some(validator) => {
                    let val_addresses =
                        self.val_address_for_consensus_key_hash(validator.address.clone())?;
                    for address in val_addresses {
                        if self.slashable_balance(address)? > 0 {
                            self.slash(address, 0)?.burn();
                        }
                    }
                }
                None => {}
            }
        }

        Ok(())
    }
}

#[derive(Encode, Decode)]
pub struct StakingEncoding<S: Symbol> {
    max_validators: u64,
    validators: <Pool<Address, Validator<S>, S> as State>::Encoding,
    amount_delegated: <Amount as State>::Encoding,
}

impl<S: Symbol> Default for StakingEncoding<S> {
    fn default() -> Self {
        Self {
            max_validators: MAX_VALIDATORS,
            validators: Default::default(),
            amount_delegated: Default::default(),
        }
    }
}

impl<S: Symbol> Staking<S> {
    pub fn delegate(
        &mut self,
        val_address: Address,
        delegator_address: Address,
        coins: Coin<S>,
    ) -> Result<()> {
        let _ = self.consensus_key(val_address)?;
        let mut validator = self.validators.get_mut(val_address)?;
        if validator.jailed {
            return Err(Error::Coins("Cannot delegate to jailed validator".into()));
        }
        validator.amount_staked = (validator.amount_staked + coins.amount)?;
        let mut delegator = validator.get_mut(delegator_address)?;
        self.amount_delegated = (self.amount_delegated + coins.amount)?;
        delegator.add_stake(coins)?;
        drop(delegator);
        let voting_power = validator.staked()?.into();
        drop(validator);

        self.set_potential_voting_power(val_address, voting_power)?;

        Ok(())
    }

    fn consensus_key(&self, val_address: Address) -> Result<Address> {
        let consensus_key = match self.consensus_keys.get(val_address)? {
            Some(key) => *key,
            None => return Err(Error::Coins("Validator is not declared".into())),
        };

        Ok(consensus_key)
    }

    pub fn declare(
        &mut self,
        val_address: Address,
        consensus_key: Address,
        commission: Decimal,
        validator_info: ValidatorInfo,
        coins: Coin<S>,
    ) -> Result<()> {
        let declared = self.consensus_keys.contains_key(val_address)?;
        if declared {
            return Err(Error::Coins("Validator is already declared".into()));
        }
        use rust_decimal_macros::dec;
        let max_comm: Decimal = dec!(1.0).into();
        let min_comm: Decimal = dec!(0.0).into();
        if commission < min_comm || commission > max_comm {
            return Err(Error::Coins("Commission must be between 0 and 1".into()));
        }
        self.consensus_keys
            .insert(val_address, consensus_key.into())?;

        let mut validator = self.validators.get_mut(val_address)?;
        validator.commission = commission;
        validator.info = validator_info;
        validator.address = val_address;
        drop(validator);

        self.delegate(val_address, val_address, coins)?;

        Ok(())
    }

    pub fn staked(&self) -> Result<Amount> {
        self.validators.balance()?.amount()
    }

    pub fn slash<A: Into<Amount>>(&mut self, val_address: Address, amount: A) -> Result<Coin<S>> {
        let _consensus_key = self.consensus_key(val_address)?;
        let jailed = self.get_mut(val_address)?.jailed;
        if !jailed {
            let reduction = self.slashable_balance(val_address)?;
            self.amount_delegated = (self.amount_delegated - reduction)?;
        }
        let amount = amount.into();
        let mut validator = self.get_mut(val_address)?;
        let slashed_coins = validator.slash(amount)?;
        drop(validator);

        if !jailed {
            self.set_potential_voting_power(val_address, 0)?;
        }

        Ok(slashed_coins)
    }

    pub fn slashable_balance(&mut self, val_address: Address) -> Result<Amount> {
        let mut validator = self.validators.get_mut(val_address)?;
        let mut sum: Decimal = 0.into();
        let delegator_keys = validator.delegator_keys()?;
        delegator_keys.iter().try_for_each(|k| -> Result<_> {
            let mut delegator = validator.get_mut(*k)?;
            sum = (sum + delegator.slashable_balance()?)?;

            Ok(())
        })?;

        sum.amount()
    }

    pub fn withdraw<A: Into<Amount>>(
        &mut self,
        val_address: Address,
        delegator_address: Address,
        amount: A,
    ) -> Result<Coin<S>> {
        let amount = amount.into();
        let mut validator = self.validators.get_mut(val_address)?;
        let mut delegator = validator.get_mut(delegator_address)?;
        delegator.process_unbonds()?;

        delegator.withdraw_liquid(amount)
    }

    pub fn unbond<A: Into<Amount>>(
        &mut self,
        val_address: Address,
        delegator_address: Address,
        amount: A,
    ) -> Result<()> {
        let amount = amount.into();
        let mut validator = self.validators.get_mut(val_address)?;
        let jailed = validator.jailed;
        {
            let mut delegator = validator.get_mut(delegator_address)?;
            delegator.unbond(amount)?;
        }

        if !jailed {
            self.amount_delegated = (self.amount_delegated - amount)?;
            validator.amount_staked = (validator.amount_staked - amount)?;
        }

        let vp = validator.staked()?.into();
        drop(validator);

        if !jailed {
            self.set_potential_voting_power(val_address, vp)?;
        }

        Ok(())
    }

    pub fn get(&self, val_address: Address) -> Result<PoolChild<Validator<S>, S>> {
        self.validators.get(val_address)
    }

    fn get_mut(&mut self, val_address: Address) -> Result<PoolChildMut<Address, Validator<S>, S>> {
        self.validators.get_mut(val_address)
    }

    #[query]
    pub fn delegations(
        &self,
        delegator_address: Address,
    ) -> Result<Vec<(Address, DelegationInfo)>> {
        self.validators
            .iter()?
            .map(|entry| {
                let (val_address, validator) = entry?;

                let delegator = validator.get(delegator_address)?;

                Ok((*val_address, delegator.info()?))
            })
            .collect()
    }

    #[call]
    pub fn unbond_self(&mut self, val_address: Address, amount: Amount) -> Result<()> {
        let signer = self.signer()?;
        self.unbond(val_address, signer, amount)
    }

    #[call]
    pub fn declare_self(
        &mut self,
        consensus_key: Address,
        commission: Decimal,
        amount: Amount,
        validator_info: ValidatorInfo,
    ) -> Result<()> {
        let signer = self.signer()?;
        let payment = self.paid()?.take(amount)?;
        self.declare(signer, consensus_key, commission, validator_info, payment)
    }

    #[call]
    pub fn delegate_from_self(&mut self, validator_address: Address, amount: Amount) -> Result<()> {
        let signer = self.signer()?;
        let payment = self.paid()?.take(amount)?;
        self.delegate(validator_address, signer, payment)
    }

    #[call]
    pub fn take_as_funding(&mut self, validator_address: Address, amount: Amount) -> Result<()> {
        let signer = self.signer()?;
        let taken_coins = self.withdraw(validator_address, signer, amount)?;
        self.paid()?.give::<S, _>(taken_coins.amount)
    }

    fn signer(&mut self) -> Result<Address> {
        self.context::<Signer>()
            .ok_or_else(|| Error::Coins("No Signer context available".into()))?
            .signer
            .ok_or_else(|| Error::Coins("Call must be signed".into()))
    }

    fn paid(&mut self) -> Result<&mut Paid> {
        self.context::<Paid>()
            .ok_or_else(|| Error::Coins("No Payment context available".into()))
    }

    fn set_potential_voting_power(&mut self, address: Address, power: u64) -> Result<()> {
        if let Some(last_indexed) = self.last_indexed_power.get(address)? {
            self.validators_by_power.delete(ValidatorPowerEntry {
                address_bytes: address.bytes(),
                inverted_power: u64::MAX - *last_indexed,
            })?;
        }

        self.validators_by_power.insert(ValidatorPowerEntry {
            address_bytes: address.bytes(),
            inverted_power: u64::MAX - power,
        })?;

        self.last_indexed_power.insert(address, power)
    }

    fn val_address_for_consensus_key_hash(
        &self,
        consensus_key_hash: Vec<u8>,
    ) -> Result<Vec<Address>> {
        let mut consensus_keys: Vec<(Address, Address)> = vec![];
        self.consensus_keys
            .iter()?
            .try_for_each(|entry| -> Result<()> {
                let (k, v) = entry?;
                consensus_keys.push((*k, *v));

                Ok(())
            })?;

        let val_addresses = consensus_keys
            .into_iter()
            .filter_map(|(k, v)| {
                let mut hasher = Sha256::new();
                hasher.update(v.bytes);
                let hash = hasher.finalize().to_vec();
                if hash[..20] == consensus_key_hash[..20] {
                    Some(k)
                } else {
                    None
                }
            })
            .collect();

        Ok(val_addresses)
    }

    fn end_block_step(&mut self) -> Result<()> {
        use std::collections::HashSet;
        let max_vals = self.max_validators;
        let mut new_val_entries: Vec<(Address, u64)> = vec![];
        let mut i = 0;
        // Collect the top validators by voting power
        for entry in self.validators_by_power.iter()? {
            let entry = entry?;
            let address: Address = entry.address_bytes.into();
            let new_power = entry.power();

            new_val_entries.push((address, new_power));

            i += 1;
            if i == max_vals {
                break;
            }
        }

        // Find the minimal set of updates required to send back to Tendermint
        let mut new_power_updates = vec![];
        for (address, power) in new_val_entries.iter() {
            match self.last_validator_powers.get(*address)? {
                Some(prev_power) => {
                    if *power != *prev_power {
                        new_power_updates.push((*address, *power));
                    }
                }
                None => new_power_updates.push((*address, *power)),
            }
        }

        let validators_in_active_set: HashSet<_> = new_val_entries
            .iter()
            .map(|(address, _)| *address)
            .collect();

        let min_vp = new_power_updates.last().map(|update| update.1).unwrap_or(0);
        // Check for validators bumped from the active validator set
        for entry in self.last_validator_powers.iter()? {
            let (address, power) = entry?;
            if !validators_in_active_set.contains(&address) && *power < min_vp {
                new_power_updates.push((*address, 0));
            }
        }

        // Tell newly-updated validators whether they're in the active set for
        // proper fee accounting
        for (address, power) in new_power_updates.iter() {
            let mut validator = self.validators.get_mut(*address)?;
            validator.in_active_set = *power > 0;
        }

        // Map to consensus key before we send back the updates
        let mut new_power_updates_con = vec![];
        for (address, power) in new_power_updates.iter() {
            let consensus_key = self
                .consensus_keys
                .get(*address)?
                .ok_or_else(|| Error::Coins("No consensus key for validator".into()))?;
            new_power_updates_con.push((*consensus_key, *power));
        }

        let val_ctx = self
            .context::<Validators>()
            .ok_or_else(|| Error::Coins("No Validators context available".into()))?;

        for (consensus_key, power) in new_power_updates_con.into_iter() {
            val_ctx.set_voting_power(consensus_key, power);
        }

        // Update the last validator powers for use in the next block
        for (address, power) in new_power_updates.iter() {
            if *power > 0 {
                self.last_validator_powers.insert(*address, *power)?;
            } else {
                self.last_validator_powers.remove(*address)?;
            }
        }

        Ok(())
    }
}

impl<S: Symbol> Give<S> for Staking<S> {
    fn give(&mut self, coins: Coin<S>) -> Result<()> {
        self.validators.give(coins)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        context::Context,
        plugins::Time,
        store::{MapStore, Shared, Store},
    };
    use rust_decimal_macros::dec;

    #[derive(State, Debug, Clone)]
    struct Simp(());
    impl Symbol for Simp {}

    #[test]
    fn staking() -> Result<()> {
        let store = Store::new(Shared::new(MapStore::new()).into());
        let mut staking: Staking<Simp> = Staking::create(store, Default::default())?;

        let alice = [0; 32].into();
        let alice_con = [4; 32].into();
        let bob = [1; 32].into();
        let bob_con = [5; 32].into();
        let carol = [2; 32].into();
        let dave = [3; 32].into();
        let dave_con = [6; 32].into();

        Context::add(Validators::default());
        Context::add(Time::from_seconds(0));

        staking
            .give(100.into())
            .expect_err("Cannot give to empty validator set");
        assert_eq!(staking.staked()?, 0);
        staking
            .delegate(alice, alice, Coin::mint(100))
            .expect_err("Should not be able to delegate to an undeclared validator");
        staking.declare(alice, alice_con, dec!(0.0).into(), vec![].into(), 50.into())?;
        staking
            .declare(alice, alice_con, dec!(0.0).into(), vec![].into(), 50.into())
            .expect_err("Should not be able to redeclare validator");

        staking.end_block_step()?;
        assert_eq!(staking.staked()?, 50);
        staking.delegate(alice, alice, Coin::mint(50))?;
        assert_eq!(staking.staked()?, 100);
        staking.declare(bob, bob_con, dec!(0.0).into(), vec![].into(), 50.into())?;
        staking.end_block_step()?;
        assert_eq!(staking.staked()?, 150);

        staking.delegate(bob, bob, Coin::mint(250))?;
        staking.delegate(bob, carol, Coin::mint(100))?;
        staking.delegate(bob, carol, Coin::mint(200))?;
        staking.delegate(bob, dave, Coin::mint(400))?;
        assert_eq!(staking.staked()?, 1100);

        let ctx = Context::resolve::<Validators>().unwrap();
        staking.end_block_step()?;
        let alice_vp = ctx.updates.get(&alice_con.bytes).unwrap().power;
        assert_eq!(alice_vp, 100);

        let bob_vp = ctx.updates.get(&bob_con.bytes).unwrap().power;
        assert_eq!(bob_vp, 1000);

        let alice_self_delegation = staking.get(alice)?.get(alice)?.staked.amount()?;
        assert_eq!(alice_self_delegation, 100);

        let bob_self_delegation = staking.get(bob)?.get(bob)?.staked.amount()?;
        assert_eq!(bob_self_delegation, 300);

        let carol_to_bob_delegation = staking.get(bob)?.get(carol)?.staked.amount()?;
        assert_eq!(carol_to_bob_delegation, 300);

        let alice_val_balance = staking.get_mut(alice)?.staked()?;
        assert_eq!(alice_val_balance, 100);

        let bob_val_balance = staking.get_mut(bob)?.staked()?;
        assert_eq!(bob_val_balance, 1000);

        // Big block rewards, doubling all balances
        staking.give(Coin::mint(600))?;
        staking.give(Coin::mint(500))?;
        assert_eq!(staking.staked()?, 1100);

        let alice_liquid = staking.get(alice)?.get(alice)?.liquid.amount()?;
        assert_eq!(alice_liquid, 100);

        let carol_to_bob_delegation = staking.get(bob)?.get(carol)?.staked.amount()?;
        assert_eq!(carol_to_bob_delegation, 300);
        let carol_to_bob_liquid = staking.get(bob)?.get(carol)?.liquid.amount()?;
        assert_eq!(carol_to_bob_liquid, 300);

        let bob_val_balance = staking.get_mut(bob)?.staked()?;
        assert_eq!(bob_val_balance, 1000);

        let bob_vp = ctx.updates.get(&bob_con.bytes).unwrap().power;
        assert_eq!(bob_vp, 1000);

        // Bob gets slashed 50%
        let slashed_coins = staking.slash(bob, 500)?;
        assert_eq!(slashed_coins.amount, 500);
        slashed_coins.burn();

        // Make sure it's now impossible to delegate to Bob
        staking
            .delegate(bob, alice, 200.into())
            .expect_err("Should not be able to delegate to jailed validator");
        staking
            .delegate(bob, bob, 200.into())
            .expect_err("Should not be able to delegate to jailed validator");

        staking.end_block_step()?;
        // Bob has been jailed and should no longer have any voting power
        let bob_vp = ctx.updates.get(&bob_con.bytes).unwrap().power;
        assert_eq!(bob_vp, 0);

        // Bob's staked coins should no longer be present in the global staking
        // balance
        assert_eq!(staking.staked()?, 100);

        // Carol can still withdraw her 300 coins from Bob's jailed validator
        {
            staking.unbond(bob, carol, 150)?;
            assert_eq!(staking.staked()?, 100);
            staking
                .withdraw(bob, carol, 450)
                .expect_err("Should not be able to take coins before unbonding period has elapsed");
            assert_eq!(staking.staked()?, 100);
            Context::add(Time::from_seconds(10));
            let carol_recovered_coins = staking.withdraw(bob, carol, 450)?;

            assert_eq!(carol_recovered_coins.amount, 450);
        }

        {
            // Bob withdraws a third of his self-delegation
            staking.unbond(bob, bob, 100)?;
            Context::add(Time::from_seconds(20));
            let bob_recovered_coins = staking.withdraw(bob, bob, 100)?;
            assert_eq!(bob_recovered_coins.amount, 100);
            staking
                .unbond(bob, bob, 201)
                .expect_err("Should not be able to unbond more than we have staked");

            staking.unbond(bob, bob, 50)?;
            Context::add(Time::from_seconds(30));
            staking
                .withdraw(bob, bob, 501)
                .expect_err("Should not be able to take more than we have unbonded");
            staking.withdraw(bob, bob, 350)?.burn();
        }

        assert_eq!(staking.staked()?, 100);
        let alice_liquid = staking.get(alice)?.get(alice)?.liquid.amount()?;
        assert_eq!(alice_liquid, 100);
        let alice_staked = staking.get(alice)?.get(alice)?.staked.amount()?;
        assert_eq!(alice_staked, 100);

        // More block reward, but bob's delegators are jailed and should not
        // earn from it
        staking.give(Coin::mint(200))?;
        assert_eq!(staking.staked()?, 100);
        let alice_val_balance = staking.get_mut(alice)?.staked()?;
        assert_eq!(alice_val_balance, 100);
        let alice_liquid = staking.get(alice)?.get(alice)?.liquid.amount()?;
        assert_eq!(alice_liquid, 300);

        staking
            .unbond(bob, dave, 401)
            .expect_err("Dave should only have 400 unbondable coins");

        assert_eq!(staking.slashable_balance(bob)?, 200);
        staking.unbond(bob, dave, 200)?;
        // Bob slashed another 50% while Dave unbonds
        assert_eq!(staking.slashable_balance(bob)?, 200);
        staking.slash(bob, 100)?.burn();
        assert_eq!(staking.slashable_balance(bob)?, 100);
        staking
            .withdraw(bob, dave, 401)
            .expect_err("Dave cannot take coins yet");
        Context::add(Time::from_seconds(40));
        staking
            .withdraw(bob, dave, 501)
            .expect_err("Dave cannot take so many coins");
        assert_eq!(staking.slashable_balance(bob)?, 0);
        staking.withdraw(bob, dave, 500)?.burn();
        assert_eq!(staking.slashable_balance(bob)?, 0);

        assert_eq!(staking.staked()?, 100);
        staking.declare(dave, dave_con, dec!(0.0).into(), vec![].into(), 300.into())?;
        staking.end_block_step()?;
        assert_eq!(staking.staked()?, 400);
        staking.end_block_step()?;
        assert_eq!(ctx.updates.get(&alice_con.bytes).unwrap().power, 100);
        assert_eq!(ctx.updates.get(&dave_con.bytes).unwrap().power, 300);
        staking.delegate(dave, carol, 300.into())?;
        assert_eq!(staking.staked()?, 700);

        staking.end_block_step()?;
        assert_eq!(ctx.updates.get(&dave_con.bytes).unwrap().power, 600);
        staking.unbond(dave, dave, 150)?;
        assert_eq!(staking.staked()?, 550);
        staking.end_block_step()?;
        assert_eq!(ctx.updates.get(&dave_con.bytes).unwrap().power, 450);

        // Test commissions
        let edith = [7; 32].into();
        let edith_con = [201; 32].into();

        staking.declare(
            edith,
            edith_con,
            dec!(0.5).into(),
            vec![].into(),
            550.into(),
        )?;

        staking.delegate(edith, carol, 550.into())?;

        staking.get_mut(edith)?.give(500.into())?;

        let edith_liquid = staking.get(edith)?.get(edith)?.liquid.amount()?;
        assert_eq!(edith_liquid, 375);
        let carol_liquid = staking.get(edith)?.get(carol)?.liquid.amount()?;
        assert_eq!(carol_liquid, 125);

        staking.slash(dave, 0)?.burn();
        staking.end_block_step()?;
        assert_eq!(ctx.updates.get(&dave_con.bytes).unwrap().power, 0);
        staking.slash(dave, 0)?.burn();

        Ok(())
    }

    #[test]
    fn val_size_limit() -> Result<()> {
        let store = Store::new(Shared::new(MapStore::new()).into());
        let mut staking: Staking<Simp> = Staking::create(store, Default::default())?;

        Context::add(Validators::default());
        Context::add(Time::from_seconds(0));
        let ctx = Context::resolve::<Validators>().unwrap();
        staking.max_validators = 2;

        for i in 1..10 {
            staking.declare(
                [i; 32].into(),
                [i; 32].into(),
                dec!(0.0).into(),
                vec![].into(),
                Amount::new(i as u64 * 100).into(),
            )?;
        }
        staking.end_block_step()?;
        assert_eq!(staking.staked()?, 1700);
        assert!(ctx.updates.get(&[7; 32]).is_none());
        assert_eq!(ctx.updates.get(&[8; 32]).unwrap().power, 800);
        assert_eq!(ctx.updates.get(&[9; 32]).unwrap().power, 900);
        staking.give(3400.into())?;
        assert_eq!(
            staking
                .get([4; 32].into())?
                .get([4; 32].into())?
                .liquid
                .amount()?,
            0
        );
        assert_eq!(
            staking
                .get([8; 32].into())?
                .get([8; 32].into())?
                .liquid
                .amount()?,
            1600
        );
        assert_eq!(
            staking
                .get([9; 32].into())?
                .get([9; 32].into())?
                .liquid
                .amount()?,
            1800
        );

        staking.declare(
            [10; 32].into(),
            [10; 32].into(),
            dec!(0.0).into(),
            vec![].into(),
            Amount::new(1000).into(),
        )?;

        staking.end_block_step()?;

        assert_eq!(ctx.updates.get(&[8; 32]).unwrap().power, 0);
        assert_eq!(ctx.updates.get(&[9; 32]).unwrap().power, 900);
        assert_eq!(ctx.updates.get(&[10; 32]).unwrap().power, 1000);
        staking.give(1900.into())?;

        assert_eq!(
            staking
                .get([8; 32].into())?
                .get([8; 32].into())?
                .liquid
                .amount()?,
            1600
        );
        assert_eq!(
            staking
                .get([9; 32].into())?
                .get([9; 32].into())?
                .liquid
                .amount()?,
            2700
        );
        assert_eq!(
            staking
                .get([10; 32].into())?
                .get([10; 32].into())?
                .liquid
                .amount()?,
            1000
        );

        Ok(())
    }
}
