use solana;
use solana_native_loader;

use solana::bank::Bank;
use solana::genesis_block::GenesisBlock;
use solana::status_deque::Status;
#[cfg(feature = "bpf_c")]
use solana_sdk::bpf_loader;
use solana_sdk::loader_transaction::LoaderTransaction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, KeypairUtil};
use solana_sdk::system_transaction::SystemTransaction;
use solana_sdk::transaction::Transaction;
#[cfg(any(feature = "bpf_c", feature = "bpf_rust"))]
use std::env;
#[cfg(any(feature = "bpf_c", feature = "bpf_rust"))]
use std::fs::File;
#[cfg(any(feature = "bpf_c", feature = "bpf_rust"))]
use std::io::Read;
#[cfg(any(feature = "bpf_c", feature = "bpf_rust"))]
use std::path::PathBuf;

/// BPF program file extension
#[cfg(any(feature = "bpf_c", feature = "bpf_rust"))]
const PLATFORM_FILE_EXTENSION_BPF: &str = "so";
/// Create a BPF program file name
#[cfg(any(feature = "bpf_c", feature = "bpf_rust"))]
fn create_bpf_path(name: &str) -> PathBuf {
    let mut pathbuf = {
        let current_exe = env::current_exe().unwrap();
        PathBuf::from(current_exe.parent().unwrap().parent().unwrap())
    };
    pathbuf.push("bpf/");
    pathbuf.push(name);
    pathbuf.set_extension(PLATFORM_FILE_EXTENSION_BPF);
    pathbuf
}

fn check_tx_results(bank: &Bank, tx: &Transaction, result: Vec<solana::bank::Result<()>>) {
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], Ok(()));
    assert_eq!(
        bank.get_signature(&tx.last_id, &tx.signatures[0]),
        Some(Status::Complete(Ok(())))
    );
}

struct Loader {
    genesis_block: GenesisBlock,
    mint_keypair: Keypair,
    bank: Bank,
    loader: Pubkey,
}

impl Loader {
    pub fn new_dynamic(loader_name: &str) -> Self {
        let (genesis_block, mint_keypair) = GenesisBlock::new(50);
        let bank = Bank::new(&genesis_block);
        let loader = Keypair::new();

        // allocate, populate, finalize, and spawn loader

        let tx = Transaction::system_create(
            &mint_keypair,
            loader.pubkey(),
            genesis_block.last_id(),
            1,
            56, // TODO
            solana_native_loader::id(),
            0,
        );
        check_tx_results(&bank, &tx, bank.process_transactions(&vec![tx.clone()]));

        let name = String::from(loader_name);
        let tx = Transaction::loader_write(
            &loader,
            solana_native_loader::id(),
            0,
            name.as_bytes().to_vec(),
            genesis_block.last_id(),
            0,
        );
        check_tx_results(&bank, &tx, bank.process_transactions(&vec![tx.clone()]));

        let tx = Transaction::loader_finalize(
            &loader,
            solana_native_loader::id(),
            genesis_block.last_id(),
            0,
        );
        check_tx_results(&bank, &tx, bank.process_transactions(&vec![tx.clone()]));

        let tx = Transaction::system_spawn(&loader, genesis_block.last_id(), 0);
        check_tx_results(&bank, &tx, bank.process_transactions(&vec![tx.clone()]));

        Loader {
            genesis_block,
            mint_keypair,
            bank,
            loader: loader.pubkey(),
        }
    }

    pub fn new_native() -> Self {
        let (genesis_block, mint_keypair) = GenesisBlock::new(50);
        let bank = Bank::new(&genesis_block);
        let loader = solana_native_loader::id();

        Loader {
            genesis_block,
            mint_keypair,
            bank,
            loader,
        }
    }

    #[cfg(feature = "bpf_c")]
    pub fn new_bpf() -> Self {
        let (genesis_block, mint_keypair) = GenesisBlock::new(50);
        let bank = Bank::new(&genesis_block);
        let loader = bpf_loader::id();

        Loader {
            genesis_block,
            mint_keypair,
            bank,
            loader,
        }
    }
}

struct Program {
    program: Keypair,
}

impl Program {
    pub fn new(loader: &Loader, userdata: &Vec<u8>) -> Self {
        let program = Keypair::new();

        // allocate, populate, finalize and spawn program

        let tx = Transaction::system_create(
            &loader.mint_keypair,
            program.pubkey(),
            loader.genesis_block.last_id(),
            1,
            userdata.len() as u64,
            loader.loader,
            0,
        );
        check_tx_results(
            &loader.bank,
            &tx,
            loader.bank.process_transactions(&vec![tx.clone()]),
        );

        let chunk_size = 256; // Size of chunk just needs to fit into tx
        let mut offset = 0;
        for chunk in userdata.chunks(chunk_size) {
            let tx = Transaction::loader_write(
                &program,
                loader.loader,
                offset,
                chunk.to_vec(),
                loader.genesis_block.last_id(),
                0,
            );
            check_tx_results(
                &loader.bank,
                &tx,
                loader.bank.process_transactions(&vec![tx.clone()]),
            );
            offset += chunk_size as u32;
        }

        let tx = Transaction::loader_finalize(
            &program,
            loader.loader,
            loader.genesis_block.last_id(),
            0,
        );
        check_tx_results(
            &loader.bank,
            &tx,
            loader.bank.process_transactions(&vec![tx.clone()]),
        );

        let tx = Transaction::system_spawn(&program, loader.genesis_block.last_id(), 0);
        check_tx_results(
            &loader.bank,
            &tx,
            loader.bank.process_transactions(&vec![tx.clone()]),
        );

        Program { program }
    }
}

#[test]
fn test_program_native_noop() {
    solana_logger::setup();

    let loader = Loader::new_native();
    let name = String::from("noop");
    let userdata = name.as_bytes().to_vec();
    let program = Program::new(&loader, &userdata);

    // Call user program
    let tx = Transaction::new(
        &loader.mint_keypair,
        &[],
        program.program.pubkey(),
        &1u8,
        loader.genesis_block.last_id(),
        0,
    );
    check_tx_results(
        &loader.bank,
        &tx,
        loader.bank.process_transactions(&vec![tx.clone()]),
    );
}

#[test]
fn test_program_lua_move_funds() {
    solana_logger::setup();

    let loader = Loader::new_dynamic("solana_lua_loader");
    let userdata = r#"
            print("Lua Script!")
            local tokens, _ = string.unpack("I", data)
            accounts[1].tokens = accounts[1].tokens - tokens
            accounts[2].tokens = accounts[2].tokens + tokens
        "#
    .as_bytes()
    .to_vec();
    let program = Program::new(&loader, &userdata);
    let from = Keypair::new();
    let to = Keypair::new().pubkey();

    // Call user program with two accounts

    let tx = Transaction::system_create(
        &loader.mint_keypair,
        from.pubkey(),
        loader.genesis_block.last_id(),
        10,
        0,
        program.program.pubkey(),
        0,
    );
    check_tx_results(
        &loader.bank,
        &tx,
        loader.bank.process_transactions(&vec![tx.clone()]),
    );

    let tx = Transaction::system_create(
        &loader.mint_keypair,
        to,
        loader.genesis_block.last_id(),
        1,
        0,
        program.program.pubkey(),
        0,
    );
    check_tx_results(
        &loader.bank,
        &tx,
        loader.bank.process_transactions(&vec![tx.clone()]),
    );

    let tx = Transaction::new(
        &from,
        &[to],
        program.program.pubkey(),
        &10,
        loader.genesis_block.last_id(),
        0,
    );
    check_tx_results(
        &loader.bank,
        &tx,
        loader.bank.process_transactions(&vec![tx.clone()]),
    );
    assert_eq!(loader.bank.get_balance(&from.pubkey()), 0);
    assert_eq!(loader.bank.get_balance(&to), 11);
}

#[cfg(feature = "bpf_c")]
#[test]
fn test_program_builtin_bpf_noop() {
    solana_logger::setup();

    let mut file = File::open(create_bpf_path("noop")).expect("file open failed");
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();

    let loader = Loader::new_bpf();
    let program = Program::new(&loader, &elf);

    // Call user program
    let tx = Transaction::new(
        &loader.mint_keypair,
        &[],
        program.program.pubkey(),
        &vec![1u8],
        loader.genesis_block.last_id(),
        0,
    );
    check_tx_results(
        &loader.bank,
        &tx,
        loader.bank.process_transactions(&vec![tx.clone()]),
    );
}

#[cfg(feature = "bpf_c")]
#[test]
fn test_program_bpf_c() {
    solana_logger::setup();

    let programs = [
        "bpf_to_bpf",
        "multiple_static",
        "noop",
        "noop++",
        "relative_call",
        "struct_pass",
        "struct_ret",
    ];
    for program in programs.iter() {
        println!("Test program: {:?}", program);
        let mut file = File::open(create_bpf_path(program)).expect("file open failed");
        let mut elf = Vec::new();
        file.read_to_end(&mut elf).unwrap();

        let loader = Loader::new_dynamic("solana_bpf_loader");
        let program = Program::new(&loader, &elf);

        // Call user program
        let tx = Transaction::new(
            &loader.mint_keypair,
            &[],
            program.program.pubkey(),
            &vec![1u8],
            loader.genesis_block.last_id(),
            0,
        );
        check_tx_results(
            &loader.bank,
            &tx,
            loader.bank.process_transactions(&vec![tx.clone()]),
        );
    }
}

// Cannot currently build the Rust BPF program as part
// of the rest of the build due to recursive `cargo build` causing
// a build deadlock.  Therefore you must build the Rust programs
// yourself first by calling `make all` in the Rust BPF program's directory
#[cfg(feature = "bpf_rust")]
#[test]
fn test_program_bpf_rust() {
    solana_logger::setup();

    let programs = ["solana_bpf_rust_noop"];
    for program in programs.iter() {
        println!("Test program: {:?}", program);
        let mut file = File::open(create_bpf_path(program)).expect("file open failed");
        let mut elf = Vec::new();
        file.read_to_end(&mut elf).unwrap();

        let loader = Loader::new_dynamic("solana_bpf_loader");
        let program = Program::new(&loader, &elf);

        // Call user program
        let tx = Transaction::new(
            &loader.mint_keypair,
            &[],
            program.program.pubkey(),
            &vec![1u8],
            loader.genesis_block.last_id(),
            0,
        );
        check_tx_results(
            &loader.bank,
            &tx,
            loader.bank.process_transactions(&vec![tx.clone()]),
        );
    }
}
