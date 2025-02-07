use colored::*;
use dialoguer::{theme::ColorfulTheme, Confirm, Input};
use hostsfile::HostsBuilder;
use indoc::printdoc;
use shared::{
    interface_config::InterfaceConfig, prompts, Association, AssociationContents, Cidr, CidrTree,
    EndpointContents, Interface, IoErrorContext, Peer, RedeemContents, State, CLIENT_CONFIG_PATH,
    REDEEM_TRANSITION_WAIT,
};
use std::{
    fmt,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};
use structopt::StructOpt;
use wgctrl::{DeviceConfigBuilder, DeviceInfo, PeerConfigBuilder, PeerInfo};

mod data_store;
mod util;

use data_store::DataStore;
use shared::{wg, Error};
use util::{http_delete, http_get, http_post, http_put, human_duration, human_size};

#[derive(Debug, StructOpt)]
#[structopt(name = "innernet", about)]
struct Opt {
    #[structopt(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, StructOpt)]
enum Command {
    /// Install a new innernet config.
    #[structopt(alias = "redeem")]
    Install { config: PathBuf },

    /// Enumerate all innernet connections.
    #[structopt(alias = "list")]
    Show {
        #[structopt(short, long)]
        short: bool,

        #[structopt(short, long)]
        tree: bool,

        interface: Option<Interface>,
    },

    /// Bring up your local interface, and update it with latest peer list.
    Up {
        /// Enable daemon mode i.e. keep the process running, while fetching
        /// the latest peer list periodically.
        #[structopt(short, long)]
        daemon: bool,

        /// Keep fetching the latest peer list at the specified interval in
        /// seconds. Valid only in daemon mode.
        #[structopt(long, default_value = "60")]
        interval: u64,

        interface: Interface,
    },

    /// Fetch and update your local interface with the latest peer list.
    Fetch { interface: Interface },

    /// Bring down the interface (equivalent to "wg-quick down [interface]")
    Down { interface: Interface },

    /// Add a new peer.
    AddPeer { interface: Interface },

    /// Add a new CIDR.
    AddCidr { interface: Interface },

    /// Disable an enabled peer.
    DisablePeer { interface: Interface },

    /// Enable a disabled peer.
    EnablePeer { interface: Interface },

    /// Add an association between CIDRs.
    AddAssociation { interface: Interface },

    /// Delete an association between CIDRs.
    DeleteAssociation { interface: Interface },

    /// List existing assocations between CIDRs.
    ListAssociations { interface: Interface },

    /// Set the local listen port.
    SetListenPort {
        interface: Interface,

        /// Unset the local listen port to use a randomized port.
        #[structopt(short, long)]
        unset: bool,
    },

    /// Override your external endpoint that the server sends to other peers.
    OverrideEndpoint {
        interface: Interface,

        /// Unset an existing override to use the automatic endpoint discovery.
        #[structopt(short, long)]
        unset: bool,
    },
}

/// Application-level error.
#[derive(Debug, Clone)]
pub(crate) struct ClientError(String);

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

fn update_hosts_file(interface: &str, peers: &Vec<Peer>) -> Result<(), Error> {
    println!(
        "{} updating {} with the latest peers.",
        "[*]".dimmed(),
        "/etc/hosts".yellow()
    );

    let mut hosts_builder = HostsBuilder::new(format!("innernet {}", interface));
    for peer in peers {
        hosts_builder.add_hostname(
            peer.contents.ip,
            &format!("{}.{}.wg", peer.contents.name, interface),
        );
    }
    hosts_builder.write()?;

    Ok(())
}

fn install(invite: &Path) -> Result<(), Error> {
    let theme = ColorfulTheme::default();
    shared::ensure_dirs_exist(&[*CLIENT_CONFIG_PATH])?;
    let mut config = InterfaceConfig::from_file(invite)?;

    let iface = Input::with_theme(&theme)
        .with_prompt("Interface name")
        .default(config.interface.network_name.clone())
        .interact()?;

    let target_conf = CLIENT_CONFIG_PATH.join(&iface).with_extension("conf");
    if target_conf.exists() {
        return Err("An interface with this name already exists in innernet.".into());
    }

    println!("{} bringing up the interface.", "[*]".dimmed());
    wg::up(
        &iface,
        &config.interface.private_key,
        config.interface.address,
        None,
        Some((
            &config.server.public_key,
            config.server.internal_endpoint.ip(),
            config.server.external_endpoint,
        )),
    )?;

    println!("{} Generating new keypair.", "[*]".dimmed());
    let keypair = wgctrl::KeyPair::generate();

    println!(
        "{} Registering keypair with server (at {}).",
        "[*]".dimmed(),
        &config.server.internal_endpoint
    );
    http_post(
        &config.server.internal_endpoint,
        "/user/redeem",
        RedeemContents {
            public_key: keypair.public.to_base64(),
        },
    )?;

    config.interface.private_key = keypair.private.to_base64();
    config.write_to_path(&target_conf, false, Some(0o600))?;
    println!(
        "{} New keypair registered. Copied config to {}.\n",
        "[*]".dimmed(),
        target_conf.to_string_lossy().yellow()
    );
    println!(
        "{} Waiting for server's WireGuard interface to transition to new key.",
        "[*]".dimmed(),
    );
    thread::sleep(*REDEEM_TRANSITION_WAIT);

    DeviceConfigBuilder::new()
        .set_private_key(keypair.private)
        .apply(&iface)?;

    fetch(&iface, false)?;

    if Confirm::with_theme(&theme)
        .with_prompt(&format!(
            "Delete invitation file \"{}\" now? (It's no longer needed)",
            invite.to_string_lossy().yellow()
        ))
        .default(true)
        .interact()?
    {
        std::fs::remove_file(invite).with_path(invite)?;
    }

    printdoc!(
        "
        {star} Done!

            {interface} has been {installed}.

            It's recommended to now keep the interface automatically refreshing via systemd:

                {systemctl_enable}{interface}

            See the documentation for more detailed instruction on managing your interface
            and your network.

    ",
        star = "[*]".dimmed(),
        interface = iface.yellow(),
        installed = "installed".green(),
        systemctl_enable = "systemctl enable --now innernet@".yellow(),
    );

    Ok(())
}

fn up(interface: &str, loop_interval: Option<Duration>) -> Result<(), Error> {
    loop {
        fetch(interface, true)?;
        match loop_interval {
            Some(interval) => thread::sleep(interval),
            None => break,
        }
    }

    Ok(())
}

fn fetch(interface: &str, bring_up_interface: bool) -> Result<(), Error> {
    let config = InterfaceConfig::from_interface(interface)?;
    let interface_up = if let Ok(interfaces) = DeviceInfo::enumerate() {
        interfaces.iter().any(|name| name == interface)
    } else {
        false
    };

    if !interface_up {
        if !bring_up_interface {
            return Err(format!(
                "Interface is not up. Use 'innernet up {}' instead",
                interface
            )
            .into());
        }

        println!("{} bringing up the interface.", "[*]".dimmed());
        wg::up(
            interface,
            &config.interface.private_key,
            config.interface.address,
            config.interface.listen_port,
            Some((
                &config.server.public_key,
                config.server.internal_endpoint.ip(),
                config.server.external_endpoint,
            )),
        )?
    }

    println!("{} fetching state from server.", "[*]".dimmed());
    let mut store = DataStore::open_or_create(&interface)?;
    let State { peers, cidrs } = http_get(&config.server.internal_endpoint, "/user/state")?;

    let device_info = DeviceInfo::get_by_name(&interface)?;
    let interface_public_key = device_info
        .public_key
        .as_ref()
        .map(|k| k.to_base64())
        .unwrap_or_default();
    let existing_peers = &device_info.peers;

    let peer_configs_diff = peers
        .iter()
        .filter(|peer| !peer.is_disabled && peer.public_key != interface_public_key)
        .filter_map(|peer| {
            let existing_peer = existing_peers
                .iter()
                .find(|p| p.config.public_key.to_base64() == peer.public_key);

            let change = match existing_peer {
                Some(existing_peer) => peer
                    .diff(&existing_peer.config)
                    .map(|diff| (PeerConfigBuilder::from(&diff), peer, "modified".normal())),
                None => Some((PeerConfigBuilder::from(peer), peer, "added".green())),
            };

            change.map(|(builder, peer, text)| {
                println!(
                    "    peer {} ({}...) was {}.",
                    peer.name.yellow(),
                    &peer.public_key[..10].dimmed(),
                    text
                );
                builder
            })
        })
        .collect::<Vec<PeerConfigBuilder>>();

    let mut device_config_builder = DeviceConfigBuilder::new();
    let mut device_config_changed = false;

    if !peer_configs_diff.is_empty() {
        device_config_builder = device_config_builder.add_peers(&peer_configs_diff);
        device_config_changed = true;
    }

    for peer in existing_peers {
        let public_key = peer.config.public_key.to_base64();
        if peers.iter().find(|p| p.public_key == public_key).is_none() {
            println!(
                "    peer ({}...) was {}.",
                &public_key[..10].yellow(),
                "removed".red()
            );

            device_config_builder =
                device_config_builder.remove_peer_by_key(&peer.config.public_key);
            device_config_changed = true;
        }
    }

    if device_config_changed {
        device_config_builder.apply(&interface)?;

        update_hosts_file(interface, &peers)?;

        println!(
            "\n{} updated interface {}\n",
            "[*]".dimmed(),
            interface.yellow()
        );
    } else {
        println!("{}", "    peers are already up to date.".green());
    }
    store.set_cidrs(cidrs);
    store.add_peers(peers)?;
    store.write()?;

    Ok(())
}

fn add_cidr(interface: &str) -> Result<(), Error> {
    let InterfaceConfig { server, .. } = InterfaceConfig::from_interface(interface)?;
    println!("Fetching CIDRs");
    let cidrs: Vec<Cidr> = http_get(&server.internal_endpoint, "/admin/cidrs")?;

    let cidr_request = prompts::add_cidr(&cidrs)?;

    println!("Creating CIDR...");
    let cidr: Cidr = http_post(&server.internal_endpoint, "/admin/cidrs", cidr_request)?;

    printdoc!(
        "
        CIDR \"{cidr_name}\" added.

        Right now, peers within {cidr_name} can only see peers in the same CIDR
        , and in the special \"infra\" CIDR that includes the innernet server peer.

        You'll need to add more associations for peers in diffent CIDRs to communicate.
        ",
        cidr_name = cidr.name.bold()
    );

    Ok(())
}

fn add_peer(interface: &str) -> Result<(), Error> {
    let InterfaceConfig { server, .. } = InterfaceConfig::from_interface(interface)?;
    println!("Fetching CIDRs");
    let cidrs: Vec<Cidr> = http_get(&server.internal_endpoint, "/admin/cidrs")?;
    println!("Fetching peers");
    let peers: Vec<Peer> = http_get(&server.internal_endpoint, "/admin/peers")?;
    let cidr_tree = CidrTree::new(&cidrs[..]);

    if let Some((peer_request, keypair)) = prompts::add_peer(&peers, &cidr_tree)? {
        println!("Creating peer...");
        let peer: Peer = http_post(&server.internal_endpoint, "/admin/peers", peer_request)?;
        let server_peer = peers.iter().find(|p| p.id == 1).unwrap();
        prompts::save_peer_invitation(
            interface,
            &peer,
            server_peer,
            &cidr_tree,
            keypair,
            &server.internal_endpoint,
        )?;
    } else {
        println!("exited without creating peer.");
    }

    Ok(())
}

fn enable_or_disable_peer(interface: &str, enable: bool) -> Result<(), Error> {
    let InterfaceConfig { server, .. } = InterfaceConfig::from_interface(interface)?;
    println!("Fetching peers.");
    let peers: Vec<Peer> = http_get(&server.internal_endpoint, "/admin/peers")?;

    if let Some(peer) = prompts::enable_or_disable_peer(&peers[..], enable)? {
        let Peer { id, mut contents } = peer;
        contents.is_disabled = !enable;
        http_put(
            &server.internal_endpoint,
            &format!("/admin/peers/{}", id),
            contents,
        )?;
    } else {
        println!("exited without disabling peer.");
    }

    Ok(())
}

fn add_association(interface: &str) -> Result<(), Error> {
    let InterfaceConfig { server, .. } = InterfaceConfig::from_interface(interface)?;

    println!("Fetching CIDRs");
    let cidrs: Vec<Cidr> = http_get(&server.internal_endpoint, "/admin/cidrs")?;

    if let Some((cidr1, cidr2)) = prompts::add_association(&cidrs[..])? {
        http_post(
            &server.internal_endpoint,
            "/admin/associations",
            AssociationContents {
                cidr_id_1: cidr1.id,
                cidr_id_2: cidr2.id,
            },
        )?;
    } else {
        println!("exited without adding association.");
    }

    Ok(())
}

fn delete_association(interface: &str) -> Result<(), Error> {
    let InterfaceConfig { server, .. } = InterfaceConfig::from_interface(interface)?;

    println!("Fetching CIDRs");
    let cidrs: Vec<Cidr> = http_get(&server.internal_endpoint, "/admin/cidrs")?;
    println!("Fetching associations");
    let associations: Vec<Association> =
        http_get(&server.internal_endpoint, "/admin/associations")?;

    if let Some(association) = prompts::delete_association(&associations[..], &cidrs[..])? {
        http_delete(
            &server.internal_endpoint,
            &format!("/admin/associations/{}", association.id),
        )?;
    } else {
        println!("exited without adding association.");
    }

    Ok(())
}

fn list_associations(interface: &str) -> Result<(), Error> {
    let InterfaceConfig { server, .. } = InterfaceConfig::from_interface(interface)?;
    println!("Fetching CIDRs");
    let cidrs: Vec<Cidr> = http_get(&server.internal_endpoint, "/admin/cidrs")?;
    println!("Fetching associations");
    let associations: Vec<Association> =
        http_get(&server.internal_endpoint, "/admin/associations")?;

    for association in associations {
        println!(
            "{}: {} <=> {}",
            association.id,
            &cidrs
                .iter()
                .find(|c| c.id == association.cidr_id_1)
                .unwrap()
                .name
                .yellow(),
            &cidrs
                .iter()
                .find(|c| c.id == association.cidr_id_2)
                .unwrap()
                .name
                .yellow()
        );
    }

    Ok(())
}

fn set_listen_port(interface: &str, unset: bool) -> Result<(), Error> {
    let mut config = InterfaceConfig::from_interface(interface)?;

    if let Some(listen_port) = prompts::set_listen_port(&config.interface, unset)? {
        wg::set_listen_port(interface, listen_port)?;
        println!("{} the interface is updated", "[*]".dimmed(),);

        config.interface.listen_port = listen_port;
        config.write_to_interface(interface)?;
        println!("{} the config file is updated", "[*]".dimmed(),);
    } else {
        println!("exited without updating listen port.");
    }

    Ok(())
}

fn override_endpoint(interface: &str, unset: bool) -> Result<(), Error> {
    let config = InterfaceConfig::from_interface(interface)?;
    if !unset && config.interface.listen_port.is_none() {
        println!(
            "{}: you need to set a listen port for your interface first.",
            "note".bold().yellow()
        );
        set_listen_port(interface, unset)?;
    }

    if let Some(endpoint) = prompts::override_endpoint(unset)? {
        println!("Updating endpoint.");
        http_put(
            &config.server.internal_endpoint,
            "/user/endpoint",
            EndpointContents::from(endpoint),
        )?;
    } else {
        println!("exited without overriding endpoint.");
    }

    Ok(())
}

fn show(short: bool, tree: bool, interface: Option<Interface>) -> Result<(), Error> {
    let interfaces = interface.map_or_else(
        || DeviceInfo::enumerate(),
        |interface| Ok(vec![interface.to_string()]),
    )?;

    let devices = interfaces.into_iter().filter_map(|name| {
        DataStore::open(&name)
            .and_then(|store| Ok((DeviceInfo::get_by_name(&name)?, store)))
            .ok()
    });
    for (mut device_info, store) in devices {
        let peers = store.peers();
        let cidrs = store.cidrs();
        let me = peers
            .iter()
            .find(|p| p.public_key == device_info.public_key.as_ref().unwrap().to_base64())
            .ok_or("missing peer info")?;

        print_interface(&device_info, me, short)?;
        // Sort the peers by last handshake time (descending),
        // then by IP address (ascending)
        device_info.peers.sort_by_key(|peer| {
            let our_peer = peers
                .iter()
                .find(|p| p.public_key == peer.config.public_key.to_base64())
                .ok_or("missing peer info")
                .unwrap();

            (
                std::cmp::Reverse(peer.stats.last_handshake_time),
                our_peer.ip,
            )
        });

        if tree {
            let cidr_tree = CidrTree::new(&cidrs[..]);
            print_tree(&cidr_tree, &peers, 1);
        } else {
            for peer in device_info.peers {
                let our_peer = peers
                    .iter()
                    .find(|p| p.public_key == peer.config.public_key.to_base64())
                    .ok_or("missing peer info")?;
                print_peer(our_peer, &peer, short)?;
            }
        }
    }
    Ok(())
}

fn print_tree(cidr: &CidrTree, peers: &[Peer], level: usize) {
    println!(
        "{:pad$}{} {}",
        "",
        cidr.cidr.to_string().bold().blue(),
        cidr.name.blue(),
        pad = level * 2
    );

    cidr.children()
        .for_each(|child| print_tree(&child, peers, level + 1));

    for peer in peers.iter().filter(|p| p.cidr_id == cidr.id) {
        println!(
            "{:pad$}| {} {}",
            "",
            peer.ip.to_string().yellow().bold(),
            peer.name.yellow(),
            pad = level * 2
        );
    }
}

fn print_interface(device_info: &DeviceInfo, me: &Peer, short: bool) -> Result<(), Error> {
    let public_key = device_info
        .public_key
        .as_ref()
        .ok_or("interface has no private key set.")?
        .to_base64();

    if short {
        println!("{}", device_info.name.green().bold());
        println!(
            "  {} {}: {} ({}...)",
            "(you)".bold(),
            me.ip.to_string().yellow().bold(),
            me.name.yellow(),
            public_key[..10].dimmed()
        );
    } else {
        println!(
            "{}: {} ({}...)",
            "interface".green().bold(),
            device_info.name.green(),
            public_key[..10].yellow()
        );
        if !short {
            if let Some(listen_port) = device_info.listen_port {
                println!("  {}: {}", "listening_port".bold(), listen_port);
            }
            println!("  {}: {}", "ip".bold(), me.ip);
        }
    }
    Ok(())
}

fn print_peer(our_peer: &Peer, peer: &PeerInfo, short: bool) -> Result<(), Error> {
    if short {
        println!(
            "  {}: {} ({}...)",
            peer.config.allowed_ips[0]
                .address
                .to_string()
                .yellow()
                .bold(),
            our_peer.name.yellow(),
            &our_peer.public_key[..10].dimmed()
        );
    } else {
        println!(
            "{}: {} ({}...)",
            "peer".yellow().bold(),
            our_peer.name.yellow(),
            &our_peer.public_key[..10].yellow()
        );
        println!("  {}: {}", "ip".bold(), our_peer.ip);
        if let Some(endpoint) = our_peer.endpoint {
            println!("  {}: {}", "endpoint".bold(), endpoint);
        }
        if let Some(last_handshake) = peer.stats.last_handshake_time {
            let duration = last_handshake.elapsed()?;
            println!(
                "  {}: {}",
                "last handshake".bold(),
                human_duration(duration),
            );
        }
        if peer.stats.tx_bytes > 0 || peer.stats.rx_bytes > 0 {
            println!(
                "  {}: {} received, {} sent",
                "transfer".bold(),
                human_size(peer.stats.rx_bytes),
                human_size(peer.stats.tx_bytes),
            );
        }
    }

    Ok(())
}

fn main() {
    let opt = Opt::from_args();

    if let Err(e) = run(opt) {
        eprintln!("\n{} {}\n", "[ERROR]".red(), e);
        std::process::exit(1);
    }
}

fn run(opt: Opt) -> Result<(), Error> {
    if unsafe { libc::getuid() } != 0 {
        return Err("innernet must run as root.".into());
    }

    let command = opt.command.unwrap_or(Command::Show {
        short: false,
        tree: false,
        interface: None,
    });

    match command {
        Command::Install { config } => install(&config)?,
        Command::Show {
            short,
            tree,
            interface,
        } => show(short, tree, interface)?,
        Command::Fetch { interface } => fetch(&interface, false)?,
        Command::Up {
            interface,
            daemon,
            interval,
        } => up(&interface, daemon.then(|| Duration::from_secs(interval)))?,
        Command::Down { interface } => wg::down(&interface)?,
        Command::AddPeer { interface } => add_peer(&interface)?,
        Command::AddCidr { interface } => add_cidr(&interface)?,
        Command::DisablePeer { interface } => enable_or_disable_peer(&interface, false)?,
        Command::EnablePeer { interface } => enable_or_disable_peer(&interface, true)?,
        Command::AddAssociation { interface } => add_association(&interface)?,
        Command::DeleteAssociation { interface } => delete_association(&interface)?,
        Command::ListAssociations { interface } => list_associations(&interface)?,
        Command::SetListenPort { interface, unset } => set_listen_port(&interface, unset)?,
        Command::OverrideEndpoint { interface, unset } => override_endpoint(&interface, unset)?,
    }

    Ok(())
}
