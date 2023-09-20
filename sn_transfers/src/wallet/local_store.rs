// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.
use super::{
    keys::{get_main_key, store_new_keypair},
    wallet_file::{
        create_received_cash_notes_dir, get_unconfirmed_txs, get_wallet, load_cash_note,
        load_received_cash_notes, store_created_cash_notes, store_unconfirmed_txs, store_wallet,
    },
    CashNoteRedemption, KeyLessWallet, Result, Transfer,
};

use crate::client_transfers::{
    create_offline_transfer, ContentPaymentsIdMap, SpendRequest, TransferOutputs,
};
use crate::{
    random_derivation_index, CashNote, DerivationIndex, DerivedSecretKey, Hash, MainPubkey,
    MainSecretKey, Nano, UniquePubkey,
};
use itertools::Itertools;
use xor_name::XorName;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

const WALLET_DIR_NAME: &str = "wallet";

/// A wallet that can only receive tokens.
pub struct LocalWallet {
    /// The secret key with which we can access
    /// all the tokens in the available_cash_notes.
    key: MainSecretKey,
    /// The wallet containing all data.
    wallet: KeyLessWallet,
    /// The dir of the wallet file, main key, public address, and new cash_notes.
    wallet_dir: PathBuf,
    /// These have not yet been successfully confirmed in
    /// the network and need to be republished, to reach network validity.
    /// We maintain the order they were added in, as to republish
    /// them in the correct order, in case any later spend was
    /// dependent on an earlier spend.
    unconfirmed_txs: BTreeSet<SpendRequest>,
}

impl LocalWallet {
    /// Stores the wallet to disk.
    pub fn store(&self) -> Result<()> {
        store_wallet(&self.wallet_dir, &self.wallet)
    }

    /// Stores the given cash_notes to the `created cash_notes dir` in the wallet dir.
    /// These can then be sent to the recipients out of band, over any channel preferred.
    pub fn store_cash_note(&mut self, cash_note: &CashNote) -> Result<()> {
        store_created_cash_notes(vec![cash_note], &self.wallet_dir)
    }

    /// Stores the given cash_notes to the `created cash_notes dir` in the wallet dir.
    /// These can then be sent to the recipients out of band, over any channel preferred.
    pub fn store_cash_notes(&mut self, cash_note: Vec<&CashNote>) -> Result<()> {
        store_created_cash_notes(cash_note, &self.wallet_dir)
    }

    pub fn get_cash_note(&mut self, unique_pubkey: &UniquePubkey) -> Option<CashNote> {
        load_cash_note(unique_pubkey, &self.wallet_dir)
    }

    /// Store unconfirmed_txs to disk.
    pub fn store_unconfirmed_txs(&mut self) -> Result<()> {
        store_unconfirmed_txs(&self.wallet_dir, self.unconfirmed_txs())
    }

    /// Unconfirmed txs exist
    pub fn unconfirmed_txs_exist(&self) -> bool {
        !self.unconfirmed_txs.is_empty()
    }

    /// Try to load any new cash_notes from the `received cash_notes dir` in the wallet dir.
    pub fn try_load_deposits(&mut self) -> Result<()> {
        let deposited = load_received_cash_notes(&self.wallet_dir)?;
        self.deposit(&deposited)?;
        Ok(())
    }

    /// Loads a serialized wallet from a path and given main key.
    pub fn load_from_main_key(root_dir: &Path, main_key: MainSecretKey) -> Result<Self> {
        let wallet_dir = root_dir.join(WALLET_DIR_NAME);
        // This creates the received_cash_notes dir if it doesn't exist.
        std::fs::create_dir_all(&wallet_dir)?;
        // This creates the main_key file if it doesn't exist.
        let (key, wallet, unconfirmed_txs) = load_from_path(&wallet_dir, Some(main_key))?;
        Ok(Self {
            key,
            wallet,
            wallet_dir: wallet_dir.to_path_buf(),
            unconfirmed_txs,
        })
    }

    /// Loads a serialized wallet from a path.
    pub fn load_from(root_dir: &Path) -> Result<Self> {
        let wallet_dir = root_dir.join(WALLET_DIR_NAME);
        // This creates the received_cash_notes dir if it doesn't exist.
        std::fs::create_dir_all(&wallet_dir)?;
        let (key, wallet, unconfirmed_txs) = load_from_path(&wallet_dir, None)?;
        Ok(Self {
            key,
            wallet,
            wallet_dir: wallet_dir.to_path_buf(),
            unconfirmed_txs,
        })
    }

    /// Tries to loads a serialized wallet from a path, bailing out if it doesn't exist.
    pub fn try_load_from(root_dir: &Path) -> Result<Self> {
        let wallet_dir = root_dir.join(WALLET_DIR_NAME);
        let (key, wallet, unconfirmed_txs) = load_from_path(&wallet_dir, None)?;
        Ok(Self {
            key,
            wallet,
            wallet_dir: wallet_dir.to_path_buf(),
            unconfirmed_txs,
        })
    }

    pub fn address(&self) -> MainPubkey {
        self.key.main_pubkey()
    }

    pub fn unconfirmed_txs(&self) -> &BTreeSet<SpendRequest> {
        &self.unconfirmed_txs
    }

    pub fn clear_unconfirmed_txs(&mut self) {
        self.unconfirmed_txs = Default::default();
    }

    pub fn balance(&self) -> Nano {
        self.wallet.balance()
    }

    pub fn sign(&self, msg: &[u8]) -> bls::Signature {
        self.key.sign(msg)
    }

    pub fn available_cash_notes(&self) -> Vec<(CashNote, DerivedSecretKey)> {
        let mut available_cash_notes = vec![];

        for (id, _token) in self.wallet.available_cash_notes.iter() {
            let held_cash_note = load_cash_note(id, &self.wallet_dir);
            if let Some(cash_note) = held_cash_note {
                if let Ok(derived_key) = cash_note.derived_key(&self.key) {
                    available_cash_notes.push((cash_note.clone(), derived_key));
                } else {
                    warn!(
                        "Skipping CashNote {:?} because we don't have the key to spend it",
                        cash_note.unique_pubkey()
                    );
                }
            } else {
                warn!("Skipping CashNote {:?} because we don't have it", id);
            }
        }
        available_cash_notes
    }

    /// Add given storage payment proofs to the wallet's cache,
    /// so they can be used when uploading the paid content.
    pub fn add_content_payments_map(&mut self, proofs: ContentPaymentsIdMap) {
        self.wallet.payment_transactions.extend(proofs);
    }

    /// Return the payment cash_note ids for the given content address name if cached.
    pub fn get_payment_unique_pubkeys(&self, name: &XorName) -> Option<&Vec<UniquePubkey>> {
        self.wallet.payment_transactions.get(name)
    }

    /// Return the payment cash_note ids for the given content address name if cached.
    pub fn get_payment_cash_notes(&self, name: &XorName) -> Vec<CashNote> {
        let ids = self.get_payment_unique_pubkeys(name);
        // now grab all those cash_notes
        let mut cash_notes: Vec<CashNote> = vec![];

        if let Some(ids) = ids {
            for id in ids {
                if let Some(cash_note) = load_cash_note(id, &self.wallet_dir) {
                    cash_notes.push(cash_note);
                }
            }
        }

        cash_notes
    }

    /// Make a transfer and return all created cash_notes
    pub fn local_send(
        &mut self,
        to: Vec<(Nano, MainPubkey)>,
        reason_hash: Option<Hash>,
    ) -> Result<Vec<CashNote>> {
        let mut rng = &mut rand::rngs::OsRng;
        // create a unique key for each output
        let to_unique_keys: Vec<_> = to
            .into_iter()
            .map(|(amount, address)| (amount, address, random_derivation_index(&mut rng)))
            .collect();

        let available_cash_notes = self.available_cash_notes();
        debug!(
            "Available CashNotes for local send: {:#?}",
            available_cash_notes
        );

        let reason_hash = reason_hash.unwrap_or_default();

        let transfer = create_offline_transfer(
            available_cash_notes,
            to_unique_keys,
            self.address(),
            reason_hash,
        )?;

        let created_cash_notes = transfer.created_cash_notes.clone();

        self.update_local_wallet(transfer)?;

        Ok(created_cash_notes)
    }

    /// Performs a CashNote payment for each content address, returning outputs for each.
    pub fn local_send_storage_payment(
        &mut self,
        all_data_payments: BTreeMap<XorName, Vec<(MainPubkey, Nano)>>,
        reason_hash: Option<Hash>,
    ) -> Result<()> {
        // create a unique key for each output
        let mut to_unique_keys = BTreeMap::default();
        let mut all_payees_only = vec![];
        for (content_addr, payees) in all_data_payments.clone().into_iter() {
            let mut rng = &mut rand::thread_rng();
            let unique_key_vec: Vec<(Nano, MainPubkey, [u8; 32])> = payees
                .into_iter()
                .map(|(address, amount)| (amount, address, random_derivation_index(&mut rng)))
                .collect_vec();
            all_payees_only.extend(unique_key_vec.clone());
            to_unique_keys.insert(content_addr, unique_key_vec);
        }

        let reason_hash = reason_hash.unwrap_or_default();

        let available_cash_notes = self.available_cash_notes();
        debug!("Available CashNotes: {:#?}", available_cash_notes);
        let transfer_outputs = create_offline_transfer(
            available_cash_notes,
            all_payees_only,
            self.address(),
            reason_hash,
        )?;

        let mut all_transfers_per_address = BTreeMap::default();

        let mut used_cash_notes = std::collections::HashSet::new();

        for (content_addr, payees) in all_data_payments {
            for (payee, _token) in payees {
                if let Some(cash_note) =
                    &transfer_outputs
                        .created_cash_notes
                        .iter()
                        .find(|cash_note| {
                            cash_note.main_pubkey() == &payee
                                && !used_cash_notes.contains(&cash_note.unique_pubkey().to_bytes())
                        })
                {
                    used_cash_notes.insert(cash_note.unique_pubkey().to_bytes());
                    let cash_notes_for_content: &mut Vec<UniquePubkey> =
                        all_transfers_per_address.entry(content_addr).or_default();
                    cash_notes_for_content.push(cash_note.unique_pubkey());
                }
            }
        }

        self.update_local_wallet(transfer_outputs)?;
        println!("Transfers applied locally");

        self.wallet
            .payment_transactions
            .extend(all_transfers_per_address);

        // get the content payment map stored
        store_wallet(&self.wallet_dir, &self.wallet)?;

        Ok(())
    }

    fn update_local_wallet(&mut self, transfer: TransferOutputs) -> Result<()> {
        // First of all, update client local state.
        let spent_unique_pubkeys: BTreeSet<_> = transfer
            .tx
            .inputs
            .iter()
            .map(|input| input.unique_pubkey())
            .collect();

        // Use retain to remove spent CashNotes in one pass, improving performance
        self.wallet
            .available_cash_notes
            .retain(|k, _| !spent_unique_pubkeys.contains(k));
        for spent in spent_unique_pubkeys {
            self.wallet.spent_cash_notes.insert(spent);
        }

        if let Some(cash_note) = transfer.change_cash_note {
            self.deposit(&vec![cash_note])?;
        }

        for cash_note in &transfer.created_cash_notes {
            self.wallet
                .cash_notes_created_for_others
                .insert(cash_note.unique_pubkey());
        }
        // Store created CashNotes in a batch, improving IO performance
        self.store_cash_notes(transfer.created_cash_notes.iter().collect())?;

        for request in transfer.all_spend_requests {
            self.unconfirmed_txs.insert(request);
        }
        Ok(())
    }

    pub fn deposit(&mut self, cash_notes: &Vec<CashNote>) -> Result<()> {
        if cash_notes.is_empty() {
            return Ok(());
        }

        for cash_note in cash_notes {
            let id = cash_note.unique_pubkey();

            if let Some(_cash_note) = load_cash_note(&id, &self.wallet_dir) {
                debug!("cash_note exists");
                return Ok(());
            }

            if self.wallet.spent_cash_notes.contains(&id) {
                debug!("cash_note is spent");
                return Ok(());
            }

            if cash_note.derived_key(&self.key).is_err() {
                continue;
            }

            let token = cash_note.token()?;
            self.wallet.available_cash_notes.insert(id, token);
            self.store_cash_note(cash_note)?;
        }

        Ok(())
    }

    pub fn unwrap_transfer(&self, transfer: Transfer) -> Result<Vec<CashNoteRedemption>> {
        transfer.cashnote_redemptions(self.key.secret_key())
    }

    pub fn derive_key(&self, derivation_index: &DerivationIndex) -> DerivedSecretKey {
        self.key.derive_key(derivation_index)
    }
}

/// Loads a serialized wallet from a path.
fn load_from_path(
    wallet_dir: &Path,
    main_key: Option<MainSecretKey>,
) -> Result<(MainSecretKey, KeyLessWallet, BTreeSet<SpendRequest>)> {
    let key = match get_main_key(wallet_dir)? {
        Some(key) => key,
        None => {
            let key = main_key.unwrap_or(MainSecretKey::random());
            store_new_keypair(wallet_dir, &key)?;
            key
        }
    };
    let unconfirmed_txs = match get_unconfirmed_txs(wallet_dir)? {
        Some(unconfirmed_txs) => unconfirmed_txs,
        None => Default::default(),
    };
    let wallet = match get_wallet(wallet_dir)? {
        Some(wallet) => {
            debug!(
                "Loaded wallet from {:#?} with balance {:?}",
                wallet_dir,
                wallet.balance()
            );
            wallet
        }
        None => {
            let wallet = KeyLessWallet::new();
            store_wallet(wallet_dir, &wallet)?;
            create_received_cash_notes_dir(wallet_dir)?;
            wallet
        }
    };

    Ok((key, wallet, unconfirmed_txs))
}

impl KeyLessWallet {
    fn new() -> Self {
        Self {
            available_cash_notes: Default::default(),
            cash_notes_created_for_others: Default::default(),
            spent_cash_notes: Default::default(),
            payment_transactions: ContentPaymentsIdMap::default(),
        }
    }

    fn balance(&self) -> Nano {
        // loop through avaiable bcs and get total token count
        let mut balance = 0;
        for (_unique_pubkey, token) in self.available_cash_notes.iter() {
            balance += token.as_nano();
        }

        Nano::from(balance)
    }
}

#[cfg(test)]
mod tests {
    use super::{get_wallet, store_wallet, LocalWallet};
    use crate::{
        genesis::{create_first_cash_note_from_key, GENESIS_CASHNOTE_AMOUNT},
        wallet::{local_store::WALLET_DIR_NAME, KeyLessWallet},
        MainSecretKey, Nano, SpendAddress,
    };
    use assert_fs::TempDir;
    use eyre::Result;

    #[tokio::test]
    async fn keyless_wallet_to_and_from_file() -> Result<()> {
        let key = MainSecretKey::random();
        let mut wallet = KeyLessWallet::new();
        let genesis = create_first_cash_note_from_key(&key).expect("Genesis creation to succeed.");

        let dir = create_temp_dir();
        let wallet_dir = dir.path().to_path_buf();

        wallet
            .available_cash_notes
            .insert(genesis.unique_pubkey(), genesis.token()?);

        store_wallet(&wallet_dir, &wallet)?;

        let deserialized = get_wallet(&wallet_dir)?.expect("There to be a wallet on disk.");

        assert_eq!(GENESIS_CASHNOTE_AMOUNT, wallet.balance().as_nano());
        assert_eq!(GENESIS_CASHNOTE_AMOUNT, deserialized.balance().as_nano());

        Ok(())
    }

    #[test]
    fn wallet_basics() -> Result<()> {
        let key = MainSecretKey::random();
        let main_pubkey = key.main_pubkey();
        let dir = create_temp_dir();

        let deposit_only = LocalWallet {
            key,
            unconfirmed_txs: Default::default(),

            wallet: KeyLessWallet::new(),
            wallet_dir: dir.path().to_path_buf(),
        };

        assert_eq!(main_pubkey, deposit_only.address());
        assert_eq!(Nano::zero(), deposit_only.balance());

        assert!(deposit_only.wallet.available_cash_notes.is_empty());
        assert!(deposit_only.wallet.cash_notes_created_for_others.is_empty());
        assert!(deposit_only.wallet.spent_cash_notes.is_empty());

        Ok(())
    }

    /// -----------------------------------
    /// <-------> DepositWallet <--------->
    /// -----------------------------------

    #[tokio::test]
    async fn deposit_empty_list_does_nothing() -> Result<()> {
        let dir = create_temp_dir();

        let mut deposit_only = LocalWallet {
            key: MainSecretKey::random(),
            unconfirmed_txs: Default::default(),

            wallet: KeyLessWallet::new(),
            wallet_dir: dir.path().to_path_buf(),
        };

        deposit_only.deposit(&vec![])?;

        assert_eq!(Nano::zero(), deposit_only.balance());

        assert!(deposit_only.wallet.available_cash_notes.is_empty());
        assert!(deposit_only.wallet.cash_notes_created_for_others.is_empty());
        assert!(deposit_only.wallet.spent_cash_notes.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn deposit_adds_cash_notes_that_belongs_to_the_wallet() -> Result<()> {
        let key = MainSecretKey::random();
        let genesis = create_first_cash_note_from_key(&key).expect("Genesis creation to succeed.");
        let dir = create_temp_dir();

        let mut deposit_only = LocalWallet {
            key,
            unconfirmed_txs: Default::default(),

            wallet: KeyLessWallet::new(),
            wallet_dir: dir.path().to_path_buf(),
        };

        deposit_only.deposit(&vec![genesis])?;

        assert_eq!(GENESIS_CASHNOTE_AMOUNT, deposit_only.balance().as_nano());

        Ok(())
    }

    #[tokio::test]
    async fn deposit_does_not_add_cash_notes_not_belonging_to_the_wallet() -> Result<()> {
        let genesis = create_first_cash_note_from_key(&MainSecretKey::random())
            .expect("Genesis creation to succeed.");
        let dir = create_temp_dir();

        let mut local_wallet = LocalWallet {
            key: MainSecretKey::random(),
            unconfirmed_txs: Default::default(),

            wallet: KeyLessWallet::new(),
            wallet_dir: dir.path().to_path_buf(),
        };

        local_wallet.deposit(&vec![genesis])?;

        assert_eq!(Nano::zero(), local_wallet.balance());

        Ok(())
    }

    #[tokio::test]
    async fn deposit_is_idempotent() -> Result<()> {
        let key = MainSecretKey::random();
        let genesis_0 =
            create_first_cash_note_from_key(&key).expect("Genesis creation to succeed.");
        let genesis_1 =
            create_first_cash_note_from_key(&key).expect("Genesis creation to succeed.");
        let dir = create_temp_dir();

        let mut deposit_only = LocalWallet {
            key,
            wallet: KeyLessWallet::new(),
            unconfirmed_txs: Default::default(),
            wallet_dir: dir.path().to_path_buf(),
        };

        deposit_only.deposit(&vec![genesis_0.clone()])?;
        assert_eq!(GENESIS_CASHNOTE_AMOUNT, deposit_only.balance().as_nano());

        deposit_only.deposit(&vec![genesis_0])?;
        assert_eq!(GENESIS_CASHNOTE_AMOUNT, deposit_only.balance().as_nano());

        deposit_only.deposit(&vec![genesis_1])?;
        assert_eq!(GENESIS_CASHNOTE_AMOUNT, deposit_only.balance().as_nano());

        Ok(())
    }

    #[tokio::test]
    async fn deposit_wallet_to_and_from_file() -> Result<()> {
        let dir = create_temp_dir();
        let root_dir = dir.path().to_path_buf();

        let mut depositor = LocalWallet::load_from(&root_dir)?;
        let genesis =
            create_first_cash_note_from_key(&depositor.key).expect("Genesis creation to succeed.");
        depositor.deposit(&vec![genesis])?;
        depositor.store()?;

        let deserialized = LocalWallet::load_from(&root_dir)?;

        assert_eq!(depositor.address(), deserialized.address());
        assert_eq!(GENESIS_CASHNOTE_AMOUNT, depositor.balance().as_nano());
        assert_eq!(GENESIS_CASHNOTE_AMOUNT, deserialized.balance().as_nano());

        assert_eq!(1, depositor.wallet.available_cash_notes.len());
        assert_eq!(0, depositor.wallet.cash_notes_created_for_others.len());
        assert_eq!(0, depositor.wallet.spent_cash_notes.len());

        assert_eq!(1, deserialized.wallet.available_cash_notes.len());
        assert_eq!(0, deserialized.wallet.cash_notes_created_for_others.len());
        assert_eq!(0, deserialized.wallet.spent_cash_notes.len());

        let a_available = depositor
            .wallet
            .available_cash_notes
            .values()
            .last()
            .expect("There to be an available CashNote.");
        let b_available = deserialized
            .wallet
            .available_cash_notes
            .values()
            .last()
            .expect("There to be an available CashNote.");
        assert_eq!(a_available, b_available);

        Ok(())
    }

    /// --------------------------------
    /// <-------> SendWallet <--------->
    /// --------------------------------

    #[tokio::test]
    async fn sending_decreases_balance() -> Result<()> {
        let dir = create_temp_dir();
        let root_dir = dir.path().to_path_buf();

        let mut sender = LocalWallet::load_from(&root_dir)?;
        let sender_cash_note =
            create_first_cash_note_from_key(&sender.key).expect("Genesis creation to succeed.");
        sender.deposit(&vec![sender_cash_note])?;

        assert_eq!(GENESIS_CASHNOTE_AMOUNT, sender.balance().as_nano());

        // We send to a new address.
        let send_amount = 100;
        let recipient_key = MainSecretKey::random();
        let recipient_main_pubkey = recipient_key.main_pubkey();
        let to = vec![(Nano::from(send_amount), recipient_main_pubkey)];
        let created_cash_notes = sender.local_send(to, None)?;

        assert_eq!(1, created_cash_notes.len());
        assert_eq!(
            GENESIS_CASHNOTE_AMOUNT - send_amount,
            sender.balance().as_nano()
        );

        let recipient_cash_note = &created_cash_notes[0];
        assert_eq!(Nano::from(send_amount), recipient_cash_note.token()?);
        assert_eq!(&recipient_main_pubkey, recipient_cash_note.main_pubkey());

        Ok(())
    }

    #[tokio::test]
    async fn send_wallet_to_and_from_file() -> Result<()> {
        let dir = create_temp_dir();
        let root_dir = dir.path().to_path_buf();

        let mut sender = LocalWallet::load_from(&root_dir)?;
        let sender_cash_note =
            create_first_cash_note_from_key(&sender.key).expect("Genesis creation to succeed.");
        sender.deposit(&vec![sender_cash_note])?;

        // We send to a new address.
        let send_amount = 100;
        let recipient_key = MainSecretKey::random();
        let recipient_main_pubkey = recipient_key.main_pubkey();
        let to = vec![(Nano::from(send_amount), recipient_main_pubkey)];
        let _created_cash_notes = sender.local_send(to, None)?;

        sender.store()?;

        let deserialized = LocalWallet::load_from(&root_dir)?;

        assert_eq!(sender.address(), deserialized.address());
        assert_eq!(
            GENESIS_CASHNOTE_AMOUNT - send_amount,
            sender.balance().as_nano()
        );
        assert_eq!(
            GENESIS_CASHNOTE_AMOUNT - send_amount,
            deserialized.balance().as_nano()
        );

        assert_eq!(1, sender.wallet.available_cash_notes.len());
        assert_eq!(1, sender.wallet.cash_notes_created_for_others.len());
        assert_eq!(1, sender.wallet.spent_cash_notes.len());

        assert_eq!(1, deserialized.wallet.available_cash_notes.len());
        assert_eq!(1, deserialized.wallet.cash_notes_created_for_others.len());
        assert_eq!(1, deserialized.wallet.spent_cash_notes.len());

        let a_available = sender
            .wallet
            .available_cash_notes
            .values()
            .last()
            .expect("There to be an available CashNote.");
        let b_available = deserialized
            .wallet
            .available_cash_notes
            .values()
            .last()
            .expect("There to be an available CashNote.");
        assert_eq!(a_available, b_available);

        let a_created_for_others = &sender.wallet.cash_notes_created_for_others;
        let b_created_for_others = &deserialized.wallet.cash_notes_created_for_others;
        assert_eq!(a_created_for_others, b_created_for_others);

        let a_spent = sender
            .wallet
            .spent_cash_notes
            .iter()
            .last()
            .expect("There to be a spent CashNote.");
        let b_spent = deserialized
            .wallet
            .spent_cash_notes
            .iter()
            .last()
            .expect("There to be a spent CashNote.");
        assert_eq!(a_spent, b_spent);

        Ok(())
    }

    #[tokio::test]
    async fn store_created_cash_note_gives_file_that_try_load_deposits_can_use() -> Result<()> {
        let sender_root_dir = create_temp_dir();
        let sender_root_dir = sender_root_dir.path().to_path_buf();

        let mut sender = LocalWallet::load_from(&sender_root_dir)?;
        let sender_cash_note =
            create_first_cash_note_from_key(&sender.key).expect("Genesis creation to succeed.");
        sender.deposit(&vec![sender_cash_note])?;

        let send_amount = 100;

        // Send to a new address.
        let recipient_root_dir = create_temp_dir();
        let recipient_root_dir = recipient_root_dir.path().to_path_buf();
        let mut recipient = LocalWallet::load_from(&recipient_root_dir)?;
        let recipient_main_pubkey = recipient.key.main_pubkey();

        let to = vec![(Nano::from(send_amount), recipient_main_pubkey)];
        let created_cash_notes = sender.local_send(to, None)?;
        let cash_note = created_cash_notes[0].clone();
        let unique_pubkey = cash_note.unique_pubkey();
        sender.store_cash_note(&cash_note)?;

        let unique_pubkey_name = *SpendAddress::from_unique_pubkey(&unique_pubkey).xorname();
        let unique_pubkey_file_name = format!("{}.cash_note", hex::encode(unique_pubkey_name));

        let created_cash_notes_dir = sender_root_dir
            .join(WALLET_DIR_NAME)
            .join("created_cash_notes");
        let created_cash_note_file = created_cash_notes_dir.join(&unique_pubkey_file_name);

        let received_cash_note_dir = recipient_root_dir
            .join(WALLET_DIR_NAME)
            .join("received_cash_notes");

        std::fs::create_dir_all(&received_cash_note_dir)?;
        let received_cash_note_file = received_cash_note_dir.join(&unique_pubkey_file_name);

        // Move the created cash_note to the recipient's received_cash_notes dir.
        std::fs::rename(created_cash_note_file, received_cash_note_file)?;

        assert_eq!(0, recipient.wallet.balance().as_nano());

        recipient.try_load_deposits()?;

        assert_eq!(1, recipient.wallet.available_cash_notes.len());

        let available = recipient
            .wallet
            .available_cash_notes
            .keys()
            .last()
            .expect("There to be an available CashNote.");

        assert_eq!(available, &unique_pubkey);
        assert_eq!(send_amount, recipient.wallet.balance().as_nano());

        Ok(())
    }

    fn create_temp_dir() -> TempDir {
        TempDir::new().expect("Should be able to create a temp dir.")
    }
}
