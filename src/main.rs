#![allow(stable_features)]
#![feature(generic_associated_types)]
#![feature(type_alias_impl_trait)]

mod client;
mod server;
mod sip003;
mod stream;
mod util;

use std::{fmt::Display, path::PathBuf, rc::Rc, sync::Arc};

use clap::{Parser, Subcommand};
use monoio::net::TcpListener;
use serde::Deserialize;
use std::fs::read_to_string;
use toml::from_str;
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, fmt, prelude::*, EnvFilter};

use crate::{client::ShadowTlsClient, server::ShadowTlsServer, util::mod_tcp_conn};

#[derive(Parser, Debug, Deserialize)]
#[clap(
    author,
    version,
    about,
    long_about = "A proxy to expose real tls handshake to the firewall.\nGithub: github.com/ihciah/shadow-tls"
)]
struct Args {
    #[clap(subcommand)]
    cmd: Commands,
    #[clap(flatten)]
    opts: Opts,
    #[clap(short, long, help = "Set configuration file path")]
    config: Option<String>,
}

#[derive(Parser, Debug, Default, Clone, Deserialize)]
pub struct Opts {
    #[clap(short, long, help = "Set parallelism manually")]
    threads: Option<u8>,
    #[clap(short, long, help = "Set TCP_NODELAY")]
    nodelay: bool,
}

impl Display for Opts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.threads {
            Some(t) => {
                write!(f, "fixed {t} threads")
            }
            None => {
                write!(f, "auto adjusted threads")
            }
        }?;
        write!(f, "; nodelay: {}", self.nodelay)
    }
}

#[derive(Subcommand, Debug, Deserialize)]
enum Commands {
    #[clap(about = "Run client side")]
    Client {
        #[clap(
            long = "listen",
            default_value = "[::1]:8080",
            help = "Shadow-tls client listen address"
        )]
        listen: String,
        #[clap(
            long = "server",
            help = "Your shadow-tls server address(like 1.2.3.4:443)"
        )]
        server_addr: String,
        #[clap(long = "sni", help = "TLS handshake SNI(like cloud.tencent.com)")]
        tls_name: String,
        #[clap(long = "password", help = "Password")]
        password: String,
    },
    #[clap(about = "Run server side")]
    Server {
        #[clap(
            long = "listen",
            default_value = "[::1]:443",
            help = "Shadow-tls server listen address"
        )]
        listen: String,
        #[clap(
            long = "server",
            help = "Your data server address(like 127.0.0.1:8080)"
        )]
        server_addr: String,
        #[clap(
            long = "tls",
            help = "TLS handshake server address(with port, like cloud.tencent.com:443)"
        )]
        tls_addr: String,
        #[clap(long = "password", help = "Password")]
        password: String,
    },
}

fn read_profile(path: PathBuf) -> Option<Args> {
    Some(
        from_str::<Args>(&read_to_string(path).expect("read profile fail"))
            .expect("profile format error"),
    )
}
impl Args {
    async fn start(&self) {
        let args_from_profile = &self.config.clone().map(|p| read_profile(p.into()));

        match &self.cmd {
            Commands::Client {
                listen,
                server_addr,
                tls_name,
                password,
            } => {
                run_client(
                    listen.clone(),
                    server_addr.clone(),
                    tls_name.clone(),
                    password.clone(),
                    self.opts.clone(),
                )
                .await
                .expect("client exited");
            }

            Commands::Server {
                listen,
                server_addr,
                tls_addr,
                password,
            } => {
                run_server(
                    listen.clone(),
                    server_addr.clone(),
                    tls_addr.clone(),
                    password.clone(),
                    self.opts.clone(),
                )
                .await
                .expect("server exited");
            }
        }
    }
}

fn main() {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();
    let args = match sip003::get_sip003_arg() {
        Some(a) => Arc::new(a),
        None => Arc::new(Args::parse()),
    };
    let mut threads = Vec::new();
    let parallelism = get_parallelism(&args);
    info!("Started with parallelism {parallelism}");
    for _ in 0..parallelism {
        let args_clone = args.clone();
        let t = std::thread::spawn(move || {
            let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
                .enable_timer()
                .build()
                .expect("unable to build monoio runtime");
            rt.block_on(args_clone.start());
        });
        threads.push(t);
    }
    threads.into_iter().for_each(|t| {
        let _ = t.join();
    });
}

fn get_parallelism(args: &Args) -> usize {
    if let Some(n) = args.opts.threads {
        return n as usize;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

async fn run_client(
    listen: String,
    server_addr: String,
    tls_name: String,
    password: String,
    opts: Opts,
) -> anyhow::Result<()> {
    info!("Client is running!\nListen address: {listen}\nRemote address: {server_addr}\nTLS server name: {tls_name}\nOpts: {opts}");
    let nodelay = opts.nodelay;
    let shadow_client = Rc::new(ShadowTlsClient::new(
        &tls_name,
        server_addr,
        password,
        opts,
    )?);
    let listener = TcpListener::bind(&listen)?;
    loop {
        match listener.accept().await {
            Ok((mut conn, addr)) => {
                info!("Accepted a connection from {addr}");
                let client = shadow_client.clone();
                mod_tcp_conn(&mut conn, true, nodelay);
                monoio::spawn(async move { client.relay(conn, addr).await });
            }
            Err(e) => {
                error!("Accept failed: {e}");
            }
        }
    }
}

async fn run_server(
    listen: String,
    server_addr: String,
    tls_addr: String,
    password: String,
    opts: Opts,
) -> anyhow::Result<()> {
    info!("Server is running!\nListen address: {listen}\nRemote address: {server_addr}\nTLS server address: {tls_addr}\nOpts: {opts}");
    let nodelay = opts.nodelay;
    let shadow_server = Rc::new(ShadowTlsServer::new(tls_addr, server_addr, password, opts));
    let listener = TcpListener::bind(&listen)?;
    loop {
        match listener.accept().await {
            Ok((mut conn, addr)) => {
                info!("Accepted a connection from {addr}");
                mod_tcp_conn(&mut conn, true, nodelay);
                let server = shadow_server.clone();
                monoio::spawn(async move { server.relay(conn).await });
            }
            Err(e) => {
                error!("Accept failed: {e}");
            }
        }
    }
}
