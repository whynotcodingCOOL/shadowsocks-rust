//! This is a binary running in the local environment
//!
//! You have to provide all needed configuration attributes via command line parameters,
//! or you could specify a configuration file. The format of configuration file is defined
//! in mod `config`.

use clap::{App, Arg, ArgGroup};
use futures::future::{self, Either};
use log::{error, info};
use tokio::{self, runtime::Builder};

use shadowsocks::{
    acl::AccessControl,
    crypto::CipherType,
    plugin::PluginConfig,
    run_local,
    Config,
    ConfigType,
    Mode,
    ServerAddr,
    ServerConfig,
};

mod logging;
mod monitor;

fn main() {
    let available_ciphers = CipherType::available_ciphers();

    let mut app = App::new("shadowsocks")
        .version(shadowsocks::VERSION)
        .about("A fast tunnel proxy that helps you bypass firewalls.")
        .arg(
            Arg::with_name("VERBOSE")
                .short("v")
                .multiple(true)
                .help("Set the level of debug"),
        )
        .arg(
            Arg::with_name("UDP_ONLY")
                .short("u")
                .help("Server mode UDP_ONLY")
                .conflicts_with("TCP_AND_UDP"),
        )
        .arg(
            Arg::with_name("TCP_AND_UDP")
                .short("U")
                .help("Server mode TCP_AND_UDP")
                .conflicts_with("UDP_ONLY"),
        )
        .arg(
            Arg::with_name("CONFIG")
                .short("c")
                .long("config")
                .takes_value(true)
                .help("Specify config file"),
        )
        .arg(
            Arg::with_name("LOCAL_ADDR")
                .short("b")
                .long("local-addr")
                .takes_value(true)
                .help("Local address, listen only to this address if specified"),
        )
        .arg(
            Arg::with_name("SERVER_ADDR")
                .short("s")
                .long("server-addr")
                .takes_value(true)
                .help("Server address")
                .requires_all(&["PASSWORD", "ENCRYPT_METHOD"]),
        )
        .arg(
            Arg::with_name("PASSWORD")
                .short("k")
                .long("password")
                .takes_value(true)
                .help("Password")
                .requires_all(&["SERVER_ADDR", "ENCRYPT_METHOD"]),
        )
        .arg(
            Arg::with_name("ENCRYPT_METHOD")
                .short("m")
                .long("encrypt-method")
                .takes_value(true)
                .possible_values(&available_ciphers)
                .help("Encryption method")
                .requires_all(&["SERVER_ADDR", "PASSWORD"]),
        )
        .arg(
            Arg::with_name("PLUGIN")
                .long("plugin")
                .takes_value(true)
                .help("Enable SIP003 plugin"),
        )
        .arg(
            Arg::with_name("PLUGIN_OPT")
                .long("plugin-opts")
                .takes_value(true)
                .help("Set SIP003 plugin options")
                .requires("PLUGIN"),
        )
        .arg(
            Arg::with_name("URL")
                .long("server-url")
                .takes_value(true)
                .help("Server address in SIP002 URL"),
        )
        .group(
            ArgGroup::with_name("SERVER_CONFIG")
                .args(&["CONFIG", "SERVER_ADDR", "URL"])
                .multiple(true)
                .required(true),
        )
        .group(
            ArgGroup::with_name("LOCAL_CONFIG")
                .args(&["CONFIG", "LOCAL_ADDR"])
                .multiple(true)
                .required(true),
        )
        .arg(
            Arg::with_name("NO_DELAY")
                .long("no-delay")
                .takes_value(false)
                .help("Set no-delay option for socket"),
        )
        .arg(
            Arg::with_name("PROTOCOL")
                .long("protocol")
                .takes_value(true)
                .default_value("socks5")
                .possible_values(&["socks5", "http"])
                .help("Protocol that uses to communicates with clients"),
        )
        .arg(
            Arg::with_name("NOFILE")
                .short("n")
                .long("nofile")
                .takes_value(true)
                .help("Set RLIMIT_NOFILE with both soft and hard limit (only for *nix systems)"),
        )
        .arg(
            Arg::with_name("ACL")
                .long("acl")
                .takes_value(true)
                .help("Path to ACL (Access Control List)"),
        )
        .arg(
            Arg::with_name("IPV6_FIRST")
                .short("6")
                .help("Resovle hostname to IPv6 address first"),
        );

    if cfg!(target_os = "android") {
        app = app
            .arg(
                Arg::with_name("VPN_MODE")
                    .long("vpn")
                    .help("Enable VPN mode (only for Android)"),
            )
            .arg(
                Arg::with_name("STAT_PATH")
                    .long("stat-path")
                    .takes_value(true)
                    .help("Specify stat_path for traffic stat (only for Android)"),
            );
    }

    #[cfg(target_os = "android")]
    {
        app = app
            .arg(
                Arg::with_name("LOCAL_DNS_ADDR")
                    .long("local-dns")
                    .takes_value(true)
                    .default_value("127.0.0.1:5353")
                    .help("Specify the address of local DNS server (only for Android)"),
            )
            .arg(
                Arg::with_name("REMOTE_DNS_ADDR")
                    .long("remote-dns")
                    .takes_value(true)
                    .default_value("8.8.8.8:53")
                    .help("Specify the address of remote DNS server (only for Android)"),
            )
            .arg(
                Arg::with_name("DNS_RELAY_ADDR")
                    .long("dns-relay")
                    .takes_value(true)
                    .default_value("127.0.0.1:5450")
                    .help("Specify the address of DNS relay (only for Android)"),
            );
    }

    let matches = app.get_matches();
    drop(available_ciphers);

    let debug_level = matches.occurrences_of("VERBOSE");
    logging::init(debug_level, "sslocal");

    let config_type = match matches.value_of("PROTOCOL") {
        Some("socks5") => ConfigType::Socks5Local,
        Some("http") => ConfigType::HttpLocal,
        Some(..) => panic!("`protocol` only supports `socks5` or `http`"),
        None => ConfigType::Socks5Local,
    };

    let mut config = match matches.value_of("CONFIG") {
        Some(cpath) => match Config::load_from_file(cpath, config_type) {
            Ok(cfg) => cfg,
            Err(err) => {
                error!("{:?}", err);
                return;
            }
        },
        None => Config::new(config_type),
    };

    if let Some(svr_addr) = matches.value_of("SERVER_ADDR") {
        let password = matches.value_of("PASSWORD").expect("password");
        let method = matches.value_of("ENCRYPT_METHOD").expect("encrypt-method");

        let method = match method.parse() {
            Ok(m) => m,
            Err(err) => {
                panic!("does not support {:?} method: {:?}", method, err);
            }
        };

        let sc = ServerConfig::new(
            svr_addr
                .parse::<ServerAddr>()
                .expect("`server-addr` invalid, \"IP:Port\" or \"Domain:Port\""),
            password.to_owned(),
            method,
            None,
            None,
        );

        config.server.push(sc);
    }

    if cfg!(target_os = "android") {
        config.local_dns_path = Some("local_dns_path".to_owned());

        if let Some(stat_path) = matches.value_of("STAT_PATH") {
            config.stat_path = Some(stat_path.to_owned());
        }

        if matches.is_present("VPN_MODE") {
            config.protect_path = Some("protect_path".to_owned());
        }
    }

    #[cfg(target_os = "android")]
    {
        use std::net::SocketAddr;

        use shadowsocks::relay::socks5::Address;

        if let Some(local_dns_addr) = matches.value_of("LOCAL_DNS_ADDR") {
            let addr = local_dns_addr.parse::<SocketAddr>().expect("local dns address");
            config.local_dns_addr = Some(addr);
        }

        if let Some(remote_dns_addr) = matches.value_of("REMOTE_DNS_ADDR") {
            let addr = remote_dns_addr.parse::<Address>().expect("remote dns address");
            config.remote_dns_addr = Some(addr);
        }

        if let Some(dns_relay_addr) = matches.value_of("DNS_RELAY_ADDR") {
            let addr = dns_relay_addr.parse::<SocketAddr>().expect("dns relay address");
            config.dns_relay_addr = Some(addr);
        }
    }

    if let Some(url) = matches.value_of("URL") {
        let svr_addr = url.parse::<ServerConfig>().expect("parse `url`");
        config.server.push(svr_addr);
    }

    if let Some(local_addr) = matches.value_of("LOCAL_ADDR") {
        let local_addr = local_addr
            .parse::<ServerAddr>()
            .expect("`local-addr` should be \"IP:Port\" or \"Domain:Port\"");

        config.local = Some(local_addr);
    }

    if matches.is_present("UDP_ONLY") {
        if config.mode.enable_tcp() {
            config.mode = Mode::TcpAndUdp;
        } else {
            config.mode = Mode::UdpOnly;
        }
    }

    if matches.is_present("TCP_AND_UDP") {
        config.mode = Mode::TcpAndUdp;
    }

    if matches.is_present("NO_DELAY") {
        config.no_delay = true;
    }

    if let Some(p) = matches.value_of("PLUGIN") {
        let plugin = PluginConfig {
            plugin: p.to_owned(),
            plugin_opt: matches.value_of("PLUGIN_OPT").map(ToOwned::to_owned),
        };

        // Overrides config in file
        for svr in config.server.iter_mut() {
            svr.set_plugin(plugin.clone());
        }
    };

    if let Some(nofile) = matches.value_of("NOFILE") {
        config.nofile = Some(nofile.parse::<u64>().expect("an unsigned integer for `nofile`"));
    }

    if let Some(acl_file) = matches.value_of("ACL") {
        let acl = AccessControl::load_from_file(acl_file).expect("Load ACL file");
        config.acl = Some(acl);
    }

    if matches.is_present("IPV6_FIRST") {
        config.ipv6_first = true;
    }

    // DONE READING options

    if config.local.is_none() {
        eprintln!(
            "missing `local_address`, consider specifying it by --local-addr command line option, \
             or \"local_address\" and \"local_port\" in configuration file"
        );
        println!("{}", matches.usage());
        return;
    }

    if config.server.is_empty() {
        eprintln!(
            "missing proxy servers, consider specifying it by \
             --server-addr, --encrypt-method, --password command line option, \
                or --server-url command line option, \
                or configuration file, check more details in https://shadowsocks.org/en/config/quick-guide.html"
        );
        println!("{}", matches.usage());
        return;
    }

    info!("shadowsocks {}", shadowsocks::VERSION);

    let mut builder = Builder::new();
    if cfg!(feature = "single-threaded") {
        builder.basic_scheduler();
    } else {
        builder.threaded_scheduler();
    }
    let mut runtime = builder.enable_all().build().expect("create tokio Runtime");
    let rt_handle = runtime.handle().clone();
    runtime.block_on(async move {
        let abort_signal = monitor::create_signal_monitor();
        let server = run_local(config, rt_handle);

        tokio::pin!(abort_signal);
        tokio::pin!(server);

        match future::select(server, abort_signal).await {
            // Server future resolved without an error. This should never happen.
            Either::Left((Ok(..), ..)) => panic!("server exited unexpectly"),
            // Server future resolved with error, which are listener errors in most cases
            Either::Left((Err(err), ..)) => panic!("server exited unexpectly with {}", err),
            // The abort signal future resolved. Means we should just exit.
            Either::Right(_) => (),
        }
    })
}
