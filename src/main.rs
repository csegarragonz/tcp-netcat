use std::{
    env,
    io::{
        self,
        Read,
        Write
    },
    net::{
        TcpListener,
        TcpStream,
    },
    time::{
        UNIX_EPOCH,
        Instant,
        SystemTime,
    },
};

fn benchmark(stream: &mut TcpStream) {
    const DATA_SIZE: usize = 10;
    let data = [7u8; DATA_SIZE];

    println!("Press enter to start benchmark:");
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();

    // Payload we are sending over the wire
    let mut payload: Vec<u8> = Vec::with_capacity(std::mem::size_of::<u32>() + data.len());
    payload.extend_from_slice(&DATA_SIZE.to_le_bytes());
    payload.extend_from_slice(&data);

    let num_iterations = 1000;
    let mut latencies: Vec<u128> = Vec::with_capacity(num_iterations.try_into().unwrap());
    for _ in 0..num_iterations {
        let start = Instant::now();
        stream.write_all(&payload).unwrap();

        let mut response_payload: Vec<u8> = vec![0u8; std::mem::size_of::<u32>() + data.len()];
        stream.read_exact(&mut response_payload).unwrap();

        latencies.push(start.elapsed().as_micros());
    }

    println!("first req: {}", latencies[0]);
    latencies.sort();
    println!("p50: {}", latencies[(num_iterations as f32 * 0.5) as usize]);
    println!("p95: {}", latencies[(num_iterations as f32 * 0.95) as usize]);
    println!("p99: {}", latencies[(num_iterations as f32 * 0.99) as usize]);
}

/// This profile function sends a message through the system, and we patch
/// Nanvix to attach a timestamp in each step. Each stamp is 2 bytes to
/// encode the timestamp. Two bytes for the time stamp gives us ~65 ms per
/// step, which should be more than enough.
fn profile(stream: &mut TcpStream) {
    // This vector contains the different steps where we add a timestamp.
    let steps: Vec<&str> = vec![
        // In-path
        "gateway::recv()", // 0
        "linuxd::handle_read_request()", // 1
        "microvm::io::try_receive_from_gateway()", // 2
        "microvm::io::try_send_to_microvm()", // 3
        "microvm::mod::memory_thread::try_recv()", // 4
        "microvm::mod::vm_input::vmexit()", // 5
        "microvm::mod::vm_input::try_recv()", // 6
        "microvm::mod::vm_input::vm_write_bytes()", // 7
        // Out-path
        "microvm::mod::vm_output::try_send()", // 8
        "microvm::io::try_recv_from_microvm()", // 9
        "microvm::io::try_send_to_gateway()", // 10
        "linuxd::handle_write_request()", // 11
        "gateway::recv()", // 12
    ];

    const MAX_NUM_STEPS: usize = 16;
    const NUM_BYTES_PER_STEP: usize = 2;
    let mut data = vec![0u8; MAX_NUM_STEPS * NUM_BYTES_PER_STEP];

    println!("Press enter to start profiling:");
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();

    // Add timestamp
    let now = SystemTime::now();
    let duration = now.duration_since(UNIX_EPOCH).expect("Time went backwards");
    let timestamp_micros = duration.as_micros();
    let timestamp_u16 = (timestamp_micros & 0xFFFF) as u16;
    let timestamp_bytes = timestamp_u16.to_be_bytes();
    data[0..2].copy_from_slice(&timestamp_bytes);

    // Payload we are sending over the wire
    let mut payload: Vec<u8> = Vec::with_capacity(std::mem::size_of::<u32>() + data.len());
    let data_len: u32 = data.len().try_into().unwrap();
    payload.extend_from_slice(&data_len.to_le_bytes());
    payload.extend_from_slice(&data);
    stream.write_all(&payload).unwrap();

    let mut response_payload: Vec<u8> = vec![0u8; std::mem::size_of::<u32>() + data.len()];
    stream.read_exact(&mut response_payload).unwrap();

    // Add timestamp
    println!("Raw Payload (b4): {:?}", response_payload[4..].iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>());
    let nnow = SystemTime::now();
    let nduration = nnow.duration_since(UNIX_EPOCH).expect("Time went backwards");
    let this_step: usize = 12;
    let this_offset: usize = 4 + 2 * this_step;
    let ntimestamp_micros = nduration.as_micros();
    let ntimestamp_u16 = (ntimestamp_micros & 0xFFFF) as u16;
    let ntimestamp_bytes = ntimestamp_u16.to_be_bytes();
    response_payload[this_offset..this_offset+2].copy_from_slice(&ntimestamp_bytes);

    // Print paylaod
    println!("Raw Payload: {:?}", response_payload[4..].iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>());
    let mut first_timestamp: Option<u16> = None;
    let mut last_timestamp: Option<u16> = None;
    for (step_idx, chunk) in (0..steps.len()).zip(response_payload[4..].chunks_exact(2)) {
        let timestamp = u16::from_be_bytes([chunk[0], chunk[1]]);

        if first_timestamp.is_none() {
            first_timestamp = Some(timestamp);
        }

        print!("{:<45} | Timestamp {:5} us", steps[step_idx], timestamp);

        if let Some(last) = last_timestamp {
            let delta = timestamp.wrapping_sub(last);  // Handles wraparound
            println!(" | Delta {:5} us", delta);
        } else {
            println!(" | First Step");
        }

        last_timestamp = Some(timestamp);
    }
    println!("Total time elapsed: {} us", last_timestamp.unwrap() - first_timestamp.unwrap());
}

fn main() -> io::Result<()> {
    let addr = "127.0.0.1:8888";
    println!("Binding to {}...", addr);
    let listener = TcpListener::bind(addr)?;

    let (mut stream, connect) = listener.accept()?;
    println!("Connected to: {connect}");

    let args: Vec<String> = env::args().skip(1).collect();
    let bench_mode = args.iter().any(|arg| arg == "--bench");
    if bench_mode {
        benchmark(&mut stream);
        return Ok(())
    }

    let args: Vec<String> = env::args().skip(1).collect();
    let bench_mode = args.iter().any(|arg| arg == "--profile");
    if bench_mode {
        profile(&mut stream);
        return Ok(())
    }

    loop {
        // Read a line from stdin
        println!("Type your message and press Enter:");
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        // Trim the newline
        let input = input.trim_end();

        // Convert input to bytes
        let input_bytes = input.as_bytes();
        let size = input_bytes.len() as u32;

        let start = Instant::now();

        let size_bytes = size.to_le_bytes();  // Use little-endian format
        let mut payload: Vec<u8> = Vec::with_capacity(std::mem::size_of::<u32>() + input_bytes.len());
        payload.extend_from_slice(&size_bytes);
        payload.extend_from_slice(&input_bytes);

        // Send the actual bytes
        stream.write_all(&payload)?;
        println!("Sent combined data: {:?} (size: {})", payload, payload.len());

        // Read response size (usize)
        let mut response_bytes: Vec<u8> = vec![0u8; std::mem::size_of::<u32>() + input_bytes.len()];
        stream.read_exact(&mut response_bytes)?;

        // Read the actual response bytes
        println!("Received: {:?}", response_bytes);

        // Optionally, print as String if it's UTF-8
        if let Ok(text) = String::from_utf8(response_bytes[4..].to_vec().clone()) {
            println!("Received text: {}", text);
        } else {
            println!("Received non-UTF-8 data");
        }

        println!("Time elapsed: {} us", start.elapsed().as_micros());
    }

    // Ok(())
}
