// Test-only raw stdio relay, compiled once by the remote-control tests.
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};

fn main() {
    let home = std::env::args().nth(1).unwrap();
    let port = std::fs::read_to_string(format!("{home}/test-port")).unwrap();
    let mut reader = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    let mut writer = reader.try_clone().unwrap();
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin(), &mut writer);
        let _ = writer.shutdown(Shutdown::Write);
    });
    let mut output = io::stdout();
    let mut buffer = [0; 8192];
    while let Ok(count) = reader.read(&mut buffer) {
        if count == 0 {
            break;
        }
        if output.write_all(&buffer[..count]).is_err() || output.flush().is_err() {
            break;
        }
    }
}
