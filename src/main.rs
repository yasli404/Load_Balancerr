use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use env_logger::Env;
use log::{info, error};
use threadpool::ThreadPool;

/// Structure représentant la configuration du proxy, incluant les adresses des serveurs, adresses des serveurs hors service, le chemin de vérification de l'état, l'intervalle de vérification de l'état et la gestion de la limitation du débit.
struct ProxyConfig {
    upstream_addresses: Vec<String>,
    inactive_upstream_addresses: Arc<Mutex<Vec<String>>>,
    health_check_path: String,
    health_check_interval: Duration,
    rate_limiter: Arc<RateLimiter>,
}

/// Structure pour gérer la limitation de débit, avec un nombre maximum de requêtes par fenêtre temporelle, la taille, et un compteur de requêtes pour chaque adresse IP.
struct RateLimiter {
    max_requests: usize,
    window_size: Duration,
    request_counts: Mutex<HashMap<String, (usize, Instant)>>,
}

impl RateLimiter {
    /// Initialise une nouvelle instance de RateLimiter.
    fn new(max_requests: usize, window_size: Duration) -> Self {
        RateLimiter {
            max_requests,
            window_size,
            request_counts: Mutex::new(HashMap::new()),
        }
    }

    /// Vérifie si adresse IP a dépassé la limite de requêtes autorisées.
    fn is_rate_limited(&self, ip: &str) -> bool {
        let mut counts = self.request_counts.lock().unwrap();
        let (count, start_time) = counts.entry(ip.to_string()).or_insert((0, Instant::now()));
        if start_time.elapsed() > self.window_size {
            *count = 1;
            *start_time = Instant::now();
            true
        } else {
            if *count < self.max_requests {
                *count += 1;
                true
            } else {
                false
            }
        }
    }
}

/// Transfère données entre deux flux TCP en utilisant des canaux pour la synchronisation.
fn data_transfer(mut src: TcpStream, mut dst: TcpStream, tx: Sender<()>, rx: Receiver<()>) {
    let mut buffer = [0; 512];
    loop {
        let bytes_read = match src.read(&mut buffer) {
            Ok(0) => break,  // Connexion fermée
            Ok(n) => n,      // Données lues
            Err(_) => break, // Erreur lecture
        };

        if dst.write_all(&buffer[..bytes_read]).is_err() {
            break; // Erreur écriture
        }

        if rx.try_recv().is_ok() {
            break; // Arrêt du transfert si l'autre thread a terminé
        }
    }
    let _ = tx.send(()); // Notifie l'autre thread que celui-ci a terminé
}

/// Gère  connexion client. Vérifie la limite de débit, puis essaie de se connecter à un serveur en amont et transfère les données entre le client et le serveur en amont.
fn handle_client_connection(mut client_stream: TcpStream, config: Arc<ProxyConfig>, client_ip: String) {
    if !config.rate_limiter.is_rate_limited(&client_ip) {
        error!("Rate limit exceeded for {}", client_ip);
        let response = b"HTTP/1.1 429 Too Many Requests\r\n\r\n";
        let _ = client_stream.write_all(response);
        return;
    }

    match connect_to_upstream(&config) {
        Ok(upstream_stream) => {
            let (tx1, rx1) = channel();
            let (tx2, rx2) = channel();

            let client_to_upstream = thread::spawn({
                let client_clone = client_stream.try_clone().expect("Failed to clone client stream");
                let upstream_clone = upstream_stream.try_clone().expect("Failed to clone upstream stream");
                move || {
                    data_transfer(client_clone, upstream_clone, tx1, rx2);
                }
            });

            let upstream_to_client = thread::spawn({
                move || {
                    data_transfer(upstream_stream, client_stream, tx2, rx1);
                }
            });

            client_to_upstream.join().unwrap();
            upstream_to_client.join().unwrap();
        }
        Err(_) => {
            error!("Failed to connect to any upstream servers");
            let response = b"HTTP/1.1 502 Bad Gateway\r\n\r\n";
            let _ = client_stream.write_all(response);
        }
    }
}

/// Connexion serveur et retourne une connexion TCP
fn connect_to_upstream(config: &Arc<ProxyConfig>) -> Result<TcpStream, ()> {
    let mut inactive_upstreams = config.inactive_upstream_addresses.lock().unwrap();
    for address in &config.upstream_addresses {
        if !inactive_upstreams.contains(address) {
            match TcpStream::connect(address) {
                Ok(stream) => return Ok(stream),
                Err(_) => {
                    error!("Failed to connect to upstream {}", address);
                    inactive_upstreams.push(address.clone());
                }
            }
        }
    }
    Err(())
}

/// Vérifie périodiquement la santé des serveurs en amont en envoyant des requêtes de vérification de santé et en mettant à jour l'état des serveurs.
fn perform_health_checks(config: Arc<ProxyConfig>) {
    loop {
        {
            let mut inactive_upstreams = config.inactive_upstream_addresses.lock().unwrap();
            for address in &config.upstream_addresses {
                let health_check_url = format!("http://{}{}", address, config.health_check_path);
                match reqwest::blocking::get(&health_check_url) {
                    Ok(response) => {
                        if response.status().is_success() {
                            inactive_upstreams.retain(|a| a != address);
                            info!("{} is healthy again", address);
                        } else {
                            if !inactive_upstreams.contains(address) {
                                inactive_upstreams.push(address.clone());
                                error!("{} failed health check", address);
                            }
                        }
                    }
                    Err(_) => {
                        if !inactive_upstreams.contains(address) {
                            inactive_upstreams.push(address.clone());
                            error!("{} failed health check", address);
                        }
                    }
                }
            }
        }
        thread::sleep(config.health_check_interval);
    }
}

fn main() {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let upstream_addresses = vec![
        "127.0.0.1:8081".to_string(),
        "127.0.0.1:8082".to_string(),
    ];
    let config = Arc::new(ProxyConfig {
        upstream_addresses,
        inactive_upstream_addresses: Arc::new(Mutex::new(Vec::new())),
        health_check_path: "/health_check".to_string(),
        health_check_interval: Duration::from_secs(10),
        rate_limiter: Arc::new(RateLimiter::new(100, Duration::from_secs(60))),
    });

    let listener = TcpListener::bind("127.0.0.1:8080").unwrap();
    info!("Proxy server listening on 127.0.0.1:8080");

    let config_clone = Arc::clone(&config);
    thread::spawn(move || {
        perform_health_checks(config_clone);
    });

    let pool = ThreadPool::new(4);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let config = Arc::clone(&config);
                let client_ip = stream.peer_addr().unwrap().ip().to_string();
                pool.execute(move || {
                    handle_client_connection(stream, config, client_ip);
                });
            }
            Err(e) => {
                error!("Failed to accept connection: {}", e);
            }
        }
    }
}
