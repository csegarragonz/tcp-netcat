use anyhow::Result;
use flexi_logger::Logger;
use indicatif::{
    ProgressBar,
    ProgressStyle,
};
use log::{
    debug,
    error,
};
use nix::{
    sys::signal::{
        Signal,
        kill,
    },
    unistd::Pid,
};
use std::{
    env,
    fmt,
    fs,
    io::{
        self,
        ErrorKind,
        Read,
        Write
    },
    mem,
    net::{
        TcpListener,
        TcpStream,
    },
    process::{
        self,
        Child,
        Command,
        Stdio,
    },
    str::FromStr,
    thread,
    time::{
        UNIX_EPOCH,
        Duration,
        Instant,
        SystemTime,
    },
};

// TODO: make this a parameter?
// TODO: this is for koala9
const CLIENT_CORE_STR: &str = "0-9";
const NANVIX_LINUXD_CORE_STR: &str = "10-14";
const NANVIX_NANOVM_CORE_STR: &str = "15-19";

// TODO: make this a parameter?
const NANVIX_LINUXD_UNIX_SOCKET: &str = "/tmp/nanvix_datapath_ubench.socket";
const GATEWAY_ADDRESS: &str = "127.0.0.1:9999";

// TODO: update me when we decide where to place this (and make it relative
// to the manifest dir)
fn get_proj_root() -> String {
    "/home/csegarra/git/nanvix/nanvix".to_string()
}

struct HwLoc {
    linuxd_core_str: String,
    nanovm_core_str: String,
}

enum BenchmarkFlavour {
    ColdStart,
    WarmStart,
    EchoBreakdown,
}

impl fmt::Display for BenchmarkFlavour {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            BenchmarkFlavour::ColdStart => "cold-start",
            BenchmarkFlavour::WarmStart => "warm-start",
            BenchmarkFlavour::EchoBreakdown => "echo-breakdown",
        };
        write!(f, "{}", s)
    }
}

impl FromStr for BenchmarkFlavour {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "cold-start" => Ok(BenchmarkFlavour::ColdStart),
            "warm-start" => Ok(BenchmarkFlavour::WarmStart),
            "echo-breakdown" => Ok(BenchmarkFlavour::EchoBreakdown),
            _ => Err(format!("Invalid benchmark type: {}", s)),
        }
    }
}

struct Benchmark {
    hwloc: HwLoc,
    flavour: BenchmarkFlavour,
    gateway_address: String,
    linuxd_address: String,
    linuxd: Option<Child>,
    nanovm: Option<Child>,
    gateway: Option<TcpStream>,
}

impl Benchmark {
    fn start_gateway(&mut self) -> Result<TcpStream> {
        debug!("Binding gateway to {}...", &self.gateway_address);
        let listener = TcpListener::bind(&self.gateway_address)?;

        self.linuxd = Some(self.start_linuxd()?);

        // When connect returns, linuxd is ready to serve requests.
        let (stream, connect) = listener.accept()?;
        debug!("Connected gateway to: {connect}");

        Ok(stream)
    }

    /// Send message to gateway by prepending the message size as a u32 LE.
    fn send_to_gateway(&mut self, data: &[u8]) -> Result<()> {
        let mut payload: Vec<u8> = Vec::with_capacity(mem::size_of::<u32>() + data.len());
        let data_len: u32 = data.len().try_into().unwrap();
        payload.extend_from_slice(&data_len.to_le_bytes());
        payload.extend_from_slice(&data);

        Ok(self.gateway.as_mut().unwrap().write_all(&payload)?)
    }

    /// Read message from gateway by first parsing the length as an u32 LE.
    fn recv_from_gateway(&mut self, data_size: usize) -> Result<Vec<u8>> {
        let mut response_payload: Vec<u8> = vec![0u8; mem::size_of::<u32>() + data_size];
        self.gateway.as_mut().unwrap().read_exact(&mut response_payload)?;

        Ok(response_payload[mem::size_of::<u32>()..].to_vec())
    }

    fn start_linuxd(&self) -> Result<Child> {
        let linuxd_args: Vec<String> = vec![
            "taskset".to_string(),
            "-ac".to_string(),
            self.hwloc.linuxd_core_str.to_string(),
            format!("{}/bin/linuxd.elf", get_proj_root()),
            "-bind-addr".to_string(),
            self.linuxd_address.clone(),
            "-gateway-addr".to_string(),
            self.gateway_address.to_string(),
            "-gateway-socket-type".to_string(),
            "tcp".to_string(),
            "-log-to-file".to_string(),
        ];

        debug!("Starting linuxd with command: {}", linuxd_args.join(" "));
        let linuxd_cmd = Command::new(&linuxd_args[0])
            .args(&linuxd_args[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .current_dir(get_proj_root())
            .spawn()?;

        Ok(linuxd_cmd)
    }

    fn start_nanovm(&self) -> Result<Child> {
        let nanovm_args: Vec<String> = vec![
            "taskset".to_string(),
            "-ac".to_string(),
            self.hwloc.nanovm_core_str.to_string(),
            format!("{}/bin/microvm.elf", get_proj_root()),
            "-kernel".to_string(),
            format!("{}/bin/kernel.elf", get_proj_root()),
            "-initrd".to_string(),
            match self.flavour {
                BenchmarkFlavour::WarmStart => format!("{}/bin/echo-rust-server-nostd.elf", get_proj_root()),
                // TODO: make this a different one without expecting an EoF?
                BenchmarkFlavour::EchoBreakdown | BenchmarkFlavour::ColdStart => format!("{}/bin/echo-rust-nostd.elf", get_proj_root()),
            },
            "-gateway".to_string(),
            self.linuxd_address.clone(),
            "-log-to-file".to_string(),
        ];

        debug!("Starting nano VM with command: {}", nanovm_args.join(" "));
        let nanovm_cmd = Command::new(&nanovm_args[0])
            .args(&nanovm_args[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .current_dir(get_proj_root())
            .spawn()?;

        Ok(nanovm_cmd)
    }

    /// Main entrypoint to set-up the micro-benchmark. It starts the gateway,
    /// linuxd and the nanovm.
    pub fn setup(&mut self) {
        // This starts linuxd under the hood.
        match self.start_gateway() {
            Ok(gateway) => self.gateway = Some(gateway),
            Err(_) => {
                error!("error starting up linuxd and the gateway");
                self.cleanup();
                process::exit(1);
            }
        }
        match self.start_nanovm() {
            Ok(nanovm) => self.nanovm = Some(nanovm),
            Err(_) => {
                error!("error starting up nano vm");
                self.cleanup();
            }
        }

        // Now we are ready to run experiments by pushing messages to the
        // gateway stream.
    }

    /// Kill the different components in order.
    pub fn cleanup(&mut self) {
        if self.nanovm.is_some() {
            debug!("Sending SIGINT to nano VM");
            match kill(Pid::from_raw(self.nanovm.as_mut().unwrap().id() as i32), Signal::SIGINT) {
                Ok(_) => {},
                Err(e) => error!("error sending SIGINT to nano VM: {e:?}"),
            }
        }

        if self.linuxd.is_some() {
            debug!("Sending SIGINT to linuxd");
            match kill(Pid::from_raw(self.linuxd.as_mut().unwrap().id() as i32), Signal::SIGINT) {
                Ok(_) => {},
                Err(e) => error!("error sending linuxd to nano VM: {e:?}"),
            }
        }

        // Remove the socket file
        match fs::remove_file(&self.linuxd_address) {
            Ok(_) => debug!("removed linuxd socket at: {}", &self.linuxd_address),
            Err(ref e) if e.kind() == ErrorKind::NotFound => {
                debug!("linuxd socket not found");
            },
            Err(e) => {
                // Non-fatal error, we are cleaning-up.
                error!("failed to delete linuxd socket file (file: {} - error: {e:?})", &self.linuxd_address);
            },
        }

        // Gateway will be closed when dropped.
    }

    pub fn run_cold_start(&mut self) -> Result<()> {
        // In the cold start experiment we cleanup and set-up at every iteration.
        self.cleanup();

        // Display a progress bar
        let num_iterations = 1e3 as usize;
        let pb = ProgressBar::new(num_iterations.try_into().unwrap());
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{msg} [{bar:40.cyan/blue}] {pos}/{len} ({percent}%)")
                .expect("invrs(eval): error creating progress bar")
                .progress_chars("#>-"),
        );
        pb.set_message("Benchmark progress:");

        const DATA_SIZE: u32 = 10;
        let data = [7u8; DATA_SIZE as usize];

        // Payload we are sending over the wire
        let mut payload: Vec<u8> = Vec::with_capacity(mem::size_of::<u32>() + data.len());
        payload.extend_from_slice(&DATA_SIZE.to_le_bytes());
        payload.extend_from_slice(&data);

        let mut latencies: Vec<u128> = Vec::with_capacity(num_iterations.try_into()?);
        for iter in 0..num_iterations {
            self.linuxd_address = format!("/tmp/nanvix_coldstart_ubench_{iter}.socket");

            // Start the clock
            let start = Instant::now();
            self.setup();
            self.gateway.as_mut().unwrap().write_all(&payload)?;

            let mut response_payload: Vec<u8> = vec![0u8; mem::size_of::<u32>() + data.len()];
            self.gateway.as_mut().unwrap().read_exact(&mut response_payload)?;
            latencies.push(start.elapsed().as_micros());

            // Sanity-check the message to make sure is the same we sent.
            if response_payload != payload {
                error!("received payload does not match sent payload!");
                error!(" - sent: {payload:?}");
                error!(" - got: {response_payload:?}");
            }

            self.cleanup();
            pb.inc(1);

            // Need to give some time to clean-up
            thread::sleep(Duration::from_millis(10));
        }

        pb.finish();
        println!("First req: {} us", latencies[0]);
        latencies.sort();
        println!("p50: {} us", latencies[(num_iterations as f32 * 0.5) as usize]);
        println!("p95: {} us", latencies[(num_iterations as f32 * 0.95) as usize]);
        println!("p99: {} us", latencies[(num_iterations as f32 * 0.99) as usize]);

        Ok(())
    }

    pub fn run_warm_start(&mut self) -> Result<()> {
        // Display a progress bar
        let num_iterations = 1e4 as usize;
        let pb = ProgressBar::new(num_iterations.try_into().unwrap());
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{msg} [{bar:40.cyan/blue}] {pos}/{len} ({percent}%)")
                .expect("invrs(eval): error creating progress bar")
                .progress_chars("#>-"),
        );
        pb.set_message("Benchmark progress:");

        const DATA_SIZE: u32 = 10;
        let data = [7u8; DATA_SIZE as usize];

        // Payload we are sending over the wire
        let mut payload: Vec<u8> = Vec::with_capacity(mem::size_of::<u32>() + data.len());
        payload.extend_from_slice(&DATA_SIZE.to_le_bytes());
        payload.extend_from_slice(&data);

        let mut latencies: Vec<u128> = Vec::with_capacity(num_iterations.try_into()?);
        for _ in 0..num_iterations {
            let start = Instant::now();
            self.gateway.as_mut().unwrap().write_all(&payload)?;

            let mut response_payload: Vec<u8> = vec![0u8; mem::size_of::<u32>() + data.len()];
            self.gateway.as_mut().unwrap().read_exact(&mut response_payload)?;
            latencies.push(start.elapsed().as_micros());

            // Sanity-check the message to make sure is the same we sent.
            if response_payload != payload {
                error!("received payload does not match sent payload!");
                error!(" - sent: {payload:?}");
                error!(" - got: {response_payload:?}");
            }

            pb.inc(1);
        }

        pb.finish();
        println!("First req (includes nano VM boot time): {} us", latencies[0]);
        latencies.sort();
        println!("p50: {} us", latencies[(num_iterations as f32 * 0.5) as usize]);
        println!("p95: {} us", latencies[(num_iterations as f32 * 0.95) as usize]);
        println!("p99: {} us", latencies[(num_iterations as f32 * 0.99) as usize]);

        Ok(())
    }

    pub fn run_echo_breakdown(&mut self) -> Result<()> {
        let _steps: Vec<&str> = vec![
            // In-path
            "gateway::recv()", // 0
            "linuxd::handle_read_request()", // 1
            "microvm::io::try_receive_from_gateway()", // 2
            "microvm::io::try_send_to_microvm()", // 3
            "microvm::mod::memory_thread::try_recv()", // 4
            "microvm::mod::vm_input::vmexit()", // 5
            "microvm::mod::vm_input::vm_write_bytes()", // 7
            // Out-path
            "microvm::mod::vm_output::try_send()", // 8
            "microvm::io::try_recv_from_microvm()", // 9
            "microvm::io::try_send_to_gateway()", // 10
            "linuxd::handle_write_request()", // 11
            "gateway::recv()", // 12
        ];

        // The maximum number of steps is hard-coded in the macro definition.
        // TODO: import from there?
        let header_size = 1;
        let max_num_steps = 16;
        let data_size = header_size + max_num_steps * 2;
        let data = vec![0u8; data_size as usize];

        // TODO: add start timestamp

        // Payload we are sending over the wire
        println!("Raw Payload: {:?}", data[header_size..].iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>());
        self.send_to_gateway(&data)?;
        let response = self.recv_from_gateway(data.len())?;

        // TODO: add end timestamp

        println!("Raw Payload: {:?}", response[header_size..].iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>());
        // Print results
        let mut first_timestamp: Option<u16> = None;
        let mut last_timestamp: Option<u16> = None;
        let num_stamps = response[0] as usize;
        debug!("parsed {num_stamps} stamps");
        for (step_idx, chunk) in (0..num_stamps).zip(response[header_size..].chunks_exact(2)) {
            let timestamp = u16::from_le_bytes([chunk[0], chunk[1]]);

            if first_timestamp.is_none() {
                first_timestamp = Some(timestamp);
            }

            // print!("{step_idx} | {:<45} | Timestamp {:5} us", steps[step_idx], timestamp);
            print!("{step_idx:<2} | Timestamp {timestamp:5} us");

            if let Some(last) = last_timestamp {
                let delta = timestamp.wrapping_sub(last);  // Handles wraparound
                println!(" | Delta {delta:5} us");
            } else {
                println!(" | First Step");
            }

            last_timestamp = Some(timestamp);
        }
        if first_timestamp.is_some() && last_timestamp.is_some() {
            println!("Total time elapsed: {} us", last_timestamp.unwrap() - first_timestamp.unwrap());
        }

        Ok(())
    }
}

fn get_benchmark_flavour() -> BenchmarkFlavour {
    let bench_flavour = env::args().nth(1);
    if bench_flavour.is_none() {
        error!("usage: nanvix-bench [cold-start,warm-start,echo-breakdown]");
        process::exit(1);
    }

    let flavour = BenchmarkFlavour::from_str(&bench_flavour.unwrap());
    if !flavour.is_ok() {
        error!("usage: nanvix-bench [cold-start,warm-start,echo-breakdown]");
        process::exit(1);
    }

    flavour.unwrap()
}

/// This is the main entrypoint for the I/O micr-benchmark where we measure the
/// time to echo a message from the gateway all the way into the guest app.
/// in the nano VM and back. It supports two modes: a regular mode where we
/// re-run the requests and report p95 and p99, or a mode where we profile the
/// time each packet spends in a different part of the system.
///
/// Usage:
/// # Regular mode
/// cargo run --release
///
/// # Profile mode
/// cargo run --release -- -profile
fn main() -> Result<()> {
    // Initialize logger, and make sure we print error logs.
    Logger::try_with_env_or_str("error")
        .expect("malformed RUST_LOG environment variable")
        .start()
        .expect("failed to initialize logger");

    let hwloc = HwLoc {
        linuxd_core_str: NANVIX_LINUXD_CORE_STR.to_string(),
        nanovm_core_str: NANVIX_NANOVM_CORE_STR.to_string(),
    };
    let mut benchmark =  Benchmark {
        hwloc,
        flavour: get_benchmark_flavour(),
        gateway_address: GATEWAY_ADDRESS.to_string(),
        linuxd_address: NANVIX_LINUXD_UNIX_SOCKET.to_string(),
        linuxd: None,
        nanovm: None,
        gateway: None,
    };

    print!("Setting up {} benchmark...", benchmark.flavour);
    benchmark.setup();
    println!("done!");

    let result = match benchmark.flavour {
        BenchmarkFlavour::EchoBreakdown => {
            println!("WARNING: this benchmark requires Nanvix (re-) compilation with TIMESTAMP_MSG=yes");
            benchmark.run_echo_breakdown()
        },
        BenchmarkFlavour::ColdStart => {
            println!("WARNING: this benchmark requires Nanvix (re-) compilation with RELEASE=yes LOG_LEVEL=panic");
            benchmark.run_cold_start()
        }
        BenchmarkFlavour::WarmStart => {
            println!("WARNING: this benchmark requires Nanvix (re-) compilation with RELEASE=yes LOG_LEVEL=panic");
            benchmark.run_warm_start()
        }
    };
    match result {
        Ok(_) => {},
        Err(e) => error!("error running benchmark: {e:?}"),
    }

    print!("Cleaning up...");
    benchmark.cleanup();
    println!("done!");

    Ok(())
}
