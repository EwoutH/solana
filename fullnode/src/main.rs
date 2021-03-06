use clap::{crate_version, App, Arg, ArgMatches};
use log::*;
use solana::client::mk_client;
use solana::cluster_info::{Node, NodeInfo, FULLNODE_PORT_RANGE};
use solana::fullnode::{Fullnode, FullnodeConfig};
use solana::leader_scheduler::LeaderScheduler;
use solana::local_vote_signer_service::LocalVoteSignerService;
use solana::socketaddr;
use solana::thin_client::{poll_gossip_for_leader, ThinClient};
use solana::vote_signer_proxy::{RemoteVoteSigner, VoteSignerProxy};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, KeypairUtil};
use solana_sdk::vote_program::VoteProgram;
use solana_sdk::vote_transaction::VoteTransaction;
use std::fs::File;
use std::io::{Error, ErrorKind, Result};
use std::net::{Ipv4Addr, SocketAddr};
use std::process::exit;
use std::sync::Arc;
use std::sync::RwLock;
use std::thread::sleep;
use std::time::Duration;

fn parse_identity(matches: &ArgMatches<'_>) -> (Keypair, SocketAddr) {
    if let Some(i) = matches.value_of("identity") {
        let path = i.to_string();
        if let Ok(file) = File::open(path.clone()) {
            let parse: serde_json::Result<solana_fullnode_config::Config> =
                serde_json::from_reader(file);

            if let Ok(config_data) = parse {
                let keypair = config_data.keypair();
                let node_info = NodeInfo::new_with_pubkey_socketaddr(
                    keypair.pubkey(),
                    &config_data.bind_addr(FULLNODE_PORT_RANGE.0),
                );

                (keypair, node_info.gossip)
            } else {
                eprintln!("failed to parse {}", path);
                exit(1);
            }
        } else {
            eprintln!("failed to read {}", path);
            exit(1);
        }
    } else {
        (Keypair::new(), socketaddr!(0, 8000))
    }
}

fn create_and_fund_vote_account(
    client: &mut ThinClient,
    vote_account: Pubkey,
    node_keypair: &Arc<Keypair>,
) -> Result<()> {
    let pubkey = node_keypair.pubkey();
    let node_balance = client.poll_get_balance(&pubkey)?;
    info!("node balance is {}", node_balance);
    if node_balance < 1 {
        return Err(Error::new(
            ErrorKind::Other,
            "insufficient tokens, one token required",
        ));
    }

    // Create the vote account if necessary
    if client.poll_get_balance(&vote_account).unwrap_or(0) == 0 {
        // Need at least two tokens as one token will be spent on a vote_account_new() transaction
        if node_balance < 2 {
            error!("insufficient tokens, two tokens required");
            return Err(Error::new(
                ErrorKind::Other,
                "insufficient tokens, two tokens required",
            ));
        }
        loop {
            let last_id = client.get_last_id();
            info!("create_and_fund_vote_account last_id={:?}", last_id);
            let transaction =
                VoteTransaction::vote_account_new(node_keypair, vote_account, last_id, 1, 1);

            match client.transfer_signed(&transaction) {
                Ok(signature) => {
                    match client.poll_for_signature(&signature) {
                        Ok(_) => match client.poll_get_balance(&vote_account) {
                            Ok(balance) => {
                                info!("vote account balance: {}", balance);
                                break;
                            }
                            Err(e) => {
                                info!("Failed to get vote account balance: {:?}", e);
                            }
                        },
                        Err(e) => {
                            info!(
                                "vote_account_new signature not found: {:?}: {:?}",
                                signature, e
                            );
                        }
                    };
                }
                Err(e) => {
                    info!("Failed to send vote_account_new transaction: {:?}", e);
                }
            };
            sleep(Duration::from_secs(2));
        }
    }

    info!("Checking for vote account registration");
    let vote_account_user_data = client.get_account_userdata(&vote_account);
    if let Ok(Some(vote_account_user_data)) = vote_account_user_data {
        if let Ok(vote_state) = VoteProgram::deserialize(&vote_account_user_data) {
            if vote_state.node_id == pubkey {
                return Ok(());
            }
        }
    }

    Err(Error::new(
        ErrorKind::Other,
        "expected successful vote account registration",
    ))
}

fn main() {
    solana_logger::setup();
    solana_metrics::set_panic_hook("fullnode");

    let matches = App::new("fullnode")
        .version(crate_version!())
        .arg(
            Arg::with_name("entry_stream")
                .long("entry-stream")
                .takes_value(true)
                .value_name("UNIX DOMAIN SOCKET")
                .help("Open entry stream at this unix domain socket location")
        )
        .arg(
            Arg::with_name("identity")
                .short("i")
                .long("identity")
                .value_name("PATH")
                .takes_value(true)
                .help("Run with the identity found in FILE"),
        )
        .arg(
            Arg::with_name("init_complete_file")
                .long("init-complete-file")
                .value_name("FILE")
                .takes_value(true)
                .help("Create this file, if it doesn't already exist, once node initialization is complete"),
        )
        .arg(
            Arg::with_name("ledger")
                .short("l")
                .long("ledger")
                .value_name("DIR")
                .takes_value(true)
                .required(true)
                .help("Use DIR as persistent ledger location"),
        )
        .arg(
            Arg::with_name("network")
                .short("n")
                .long("network")
                .value_name("HOST:PORT")
                .takes_value(true)
                .help("Rendezvous with the cluster at this gossip entry point"),
        )
        .arg(
            Arg::with_name("no_leader_rotation")
                .long("no-leader-rotation")
                .help("Disable leader rotation"),
        )
        .arg(
            Arg::with_name("no_signer")
                .long("no-signer")
                .takes_value(false)
                .conflicts_with("signer")
                .help("Launch node without vote signer"),
        )
        .arg(
            Arg::with_name("no_sigverify")
                .short("v")
                .long("no-sigverify")
                .help("Run without signature verification"),
        )
        .arg(
            Arg::with_name("rpc_port")
                .long("rpc-port")
                .value_name("PORT")
                .takes_value(true)
                .help("RPC port to use for this node"),
        )
        .arg(
            Arg::with_name("signer")
                .short("s")
                .long("signer")
                .value_name("HOST:PORT")
                .takes_value(true)
                .help("Rendezvous with the vote signer at this RPC end point"),
        )
        .get_matches();

    let mut fullnode_config = FullnodeConfig::default();
    fullnode_config.sigverify_disabled = matches.is_present("no_sigverify");
    let no_signer = matches.is_present("no_signer");
    let use_only_bootstrap_leader = matches.is_present("no_leader_rotation");
    let (keypair, gossip) = parse_identity(&matches);
    let ledger_path = matches.value_of("ledger").unwrap();
    let cluster_entrypoint = matches
        .value_of("network")
        .map(|network| network.parse().expect("failed to parse network address"));
    let (_signer_service, signer_addr) = if let Some(signer_addr) = matches.value_of("signer") {
        (
            None,
            signer_addr.to_string().parse().expect("Signer IP Address"),
        )
    } else {
        // Run a local vote signer if a vote signer service address was not provided
        let (signer_service, signer_addr) = LocalVoteSignerService::new();
        (Some(signer_service), signer_addr)
    };
    let (rpc_port, rpc_pubsub_port) = if let Some(port) = matches.value_of("rpc_port") {
        let port_number = port.to_string().parse().expect("integer");
        if port_number == 0 {
            eprintln!("Invalid RPC port requested: {:?}", port);
            exit(1);
        }
        (port_number, port_number + 1)
    } else {
        (
            solana_netutil::find_available_port_in_range(FULLNODE_PORT_RANGE)
                .expect("unable to allocate rpc_port"),
            solana_netutil::find_available_port_in_range(FULLNODE_PORT_RANGE)
                .expect("unable to allocate rpc_pubsub_port"),
        )
    };
    let init_complete_file = matches.value_of("init_complete_file");
    fullnode_config.entry_stream = matches.value_of("entry_stream").map(|s| s.to_string());

    let keypair = Arc::new(keypair);
    let mut node = Node::new_with_external_ip(keypair.pubkey(), &gossip);
    node.info.rpc.set_port(rpc_port);
    node.info.rpc_pubsub.set_port(rpc_pubsub_port);

    let mut leader_scheduler = LeaderScheduler::default();
    leader_scheduler.use_only_bootstrap_leader = use_only_bootstrap_leader;

    let vote_signer_option = if !no_signer {
        let vote_signer = VoteSignerProxy::new_with_signer(
            &keypair,
            Box::new(RemoteVoteSigner::new(signer_addr)),
        );
        info!("Signer service address: {:?}", signer_addr);
        info!("New vote account ID is {:?}", vote_signer.pubkey());
        Some(Arc::new(vote_signer))
    } else {
        None
    };
    let vote_account_option = vote_signer_option.as_ref().map(|x| x.pubkey());

    let gossip_addr = node.info.gossip;
    let mut fullnode = Fullnode::new(
        node,
        keypair.clone(),
        ledger_path,
        Arc::new(RwLock::new(leader_scheduler)),
        vote_signer_option,
        cluster_entrypoint
            .map(|i| NodeInfo::new_entry_point(&i))
            .as_ref(),
        fullnode_config,
    );

    if let Some(vote_account) = vote_account_option {
        let leader_node_info = loop {
            info!("Looking for leader...");
            match poll_gossip_for_leader(gossip_addr, Some(10)) {
                Ok(leader_node_info) => {
                    info!("Found leader: {:?}", leader_node_info);
                    break leader_node_info;
                }
                Err(err) => {
                    info!("Unable to find leader: {:?}", err);
                }
            };
        };

        let mut client = mk_client(&leader_node_info);
        if let Err(err) = create_and_fund_vote_account(&mut client, vote_account, &keypair) {
            panic!("Failed to create_and_fund_vote_account: {:?}", err);
        }
    }

    if let Some(filename) = init_complete_file {
        File::create(filename).unwrap_or_else(|_| panic!("Unable to create: {}", filename));
    }
    info!("Node initialized");
    loop {
        let status = fullnode.handle_role_transition();
        match status {
            Ok(Some(transition)) => {
                info!("role_transition complete: {:?}", transition);
            }
            _ => {
                panic!(
                    "Fullnode TPU/TVU exited for some unexpected reason: {:?}",
                    status
                );
            }
        };
    }
}
