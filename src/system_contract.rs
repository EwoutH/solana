//! system smart contract

use bank::Account;
use bincode::deserialize;
use signature::Pubkey;
use transaction::Transaction;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum SystemContract {
    /// Create a new account
    /// * Transaction::keys[0] - source
    /// * Transaction::keys[1] - new account key
    /// * tokens - number of tokens to transfer to the new account
    /// * space - memory to allocate if greater then zero
    /// * contract - the contract id of the new account
    CreateAccount {
        tokens: i64,
        space: u64,
        contract_id: Option<Pubkey>,
    },
    /// Assign account to a contract
    /// * Transaction::keys[0] - account to assign
    Assign { contract_id: Pubkey },
    /// Move tokens
    /// * Transaction::keys[0] - source
    /// * Transaction::keys[1] - destination
    Move { tokens: i64 },
}

pub const SYSTEM_CONTRACT_ID: [u8; 32] = [0u8; 32];

impl SystemContract {
    pub fn check_id(contract_id: &Pubkey) -> bool {
        contract_id.as_ref() == SYSTEM_CONTRACT_ID
    }

    pub fn id() -> Pubkey {
        Pubkey::new(&SYSTEM_CONTRACT_ID)
    }
    pub fn get_balance(account: &Account) -> i64 {
        account.tokens
    }
    pub fn process_transaction(tx: &Transaction, accounts: &mut [Account]) {
        let syscall: SystemContract = deserialize(&tx.userdata).unwrap();
        match syscall {
            SystemContract::CreateAccount {
                tokens,
                space,
                contract_id,
            } => {
                if !Self::check_id(&accounts[0].contract_id) {
                    return;
                }
                if space > 0
                    && (!accounts[1].userdata.is_empty()
                        || !Self::check_id(&accounts[1].contract_id))
                {
                    return;
                }
                accounts[0].tokens -= tokens;
                accounts[1].tokens += tokens;
                if let Some(id) = contract_id {
                    accounts[1].contract_id = id;
                }
                accounts[1].userdata = vec![0; space as usize];
            }
            SystemContract::Assign { contract_id } => {
                if !Self::check_id(&accounts[0].contract_id) {
                    return;
                }
                accounts[0].contract_id = contract_id;
            }
            SystemContract::Move { tokens } => {
                //bank should be verifying correctness
                accounts[0].tokens -= tokens;
                accounts[1].tokens += tokens;
            }
        }
    }
}
#[cfg(test)]
mod test {
    use bank::Account;
    use hash::Hash;
    use signature::{Keypair, KeypairUtil};
    use system_contract::SystemContract;
    use transaction::Transaction;
    #[test]
    fn test_create_noop() {
        let from = Keypair::new();
        let to = Keypair::new();
        let mut accounts = vec![Account::default(), Account::default()];
        let tx = Transaction::system_new(&from, to.pubkey(), 0, Hash::default());
        SystemContract::process_transaction(&tx, &mut accounts);
        assert_eq!(accounts[0].tokens, 0);
        assert_eq!(accounts[1].tokens, 0);
    }
    #[test]
    fn test_create_spend() {
        let from = Keypair::new();
        let to = Keypair::new();
        let mut accounts = vec![Account::default(), Account::default()];
        accounts[0].tokens = 1;
        let tx = Transaction::system_new(&from, to.pubkey(), 1, Hash::default());
        SystemContract::process_transaction(&tx, &mut accounts);
        assert_eq!(accounts[0].tokens, 0);
        assert_eq!(accounts[1].tokens, 1);
    }
    #[test]
    fn test_create_spend_wrong_source() {
        let from = Keypair::new();
        let to = Keypair::new();
        let mut accounts = vec![Account::default(), Account::default()];
        accounts[0].tokens = 1;
        accounts[0].contract_id = from.pubkey();
        let tx = Transaction::system_new(&from, to.pubkey(), 1, Hash::default());
        SystemContract::process_transaction(&tx, &mut accounts);
        assert_eq!(accounts[0].tokens, 1);
        assert_eq!(accounts[1].tokens, 0);
    }
    #[test]
    fn test_create_assign_and_allocate() {
        let from = Keypair::new();
        let to = Keypair::new();
        let mut accounts = vec![Account::default(), Account::default()];
        let tx = Transaction::system_create(
            &from,
            to.pubkey(),
            Hash::default(),
            0,
            1,
            Some(to.pubkey()),
            0,
        );
        SystemContract::process_transaction(&tx, &mut accounts);
        assert!(accounts[0].userdata.is_empty());
        assert_eq!(accounts[1].userdata.len(), 1);
        assert_eq!(accounts[1].contract_id, to.pubkey());
    }
    #[test]
    fn test_create_allocate_wrong_dest_contract() {
        let from = Keypair::new();
        let to = Keypair::new();
        let mut accounts = vec![Account::default(), Account::default()];
        accounts[1].contract_id = to.pubkey();
        let tx = Transaction::system_create(&from, to.pubkey(), Hash::default(), 0, 1, None, 0);
        SystemContract::process_transaction(&tx, &mut accounts);
        assert!(accounts[1].userdata.is_empty());
    }
    #[test]
    fn test_create_allocate_wrong_source_contract() {
        let from = Keypair::new();
        let to = Keypair::new();
        let mut accounts = vec![Account::default(), Account::default()];
        accounts[0].contract_id = to.pubkey();
        let tx = Transaction::system_create(&from, to.pubkey(), Hash::default(), 0, 1, None, 0);
        SystemContract::process_transaction(&tx, &mut accounts);
        assert!(accounts[1].userdata.is_empty());
    }
    #[test]
    fn test_create_allocate_already_allocated() {
        let from = Keypair::new();
        let to = Keypair::new();
        let mut accounts = vec![Account::default(), Account::default()];
        accounts[1].userdata = vec![0, 0, 0];
        let tx = Transaction::system_create(&from, to.pubkey(), Hash::default(), 0, 2, None, 0);
        SystemContract::process_transaction(&tx, &mut accounts);
        assert_eq!(accounts[1].userdata.len(), 3);
    }
    #[test]
    fn test_create_assign() {
        let from = Keypair::new();
        let contract = Keypair::new();
        let mut accounts = vec![Account::default()];
        let tx = Transaction::system_assign(&from, Hash::default(), contract.pubkey(), 0);
        SystemContract::process_transaction(&tx, &mut accounts);
        assert_eq!(accounts[0].contract_id, contract.pubkey());
    }
    #[test]
    fn test_move() {
        let from = Keypair::new();
        let to = Keypair::new();
        let mut accounts = vec![Account::default(), Account::default()];
        accounts[0].tokens = 1;
        let tx = Transaction::new(&from, to.pubkey(), 1, Hash::default());
        SystemContract::process_transaction(&tx, &mut accounts);
        assert_eq!(accounts[0].tokens, 0);
        assert_eq!(accounts[1].tokens, 1);
    }
}