//! Runs the shipped reply reader in its own test process, away from unit
//! tests' native-owner globals. No game or network login is started; all
//! credentials are synthetic.

use crystal_loader::{b64, events, pipe::{random_pipe_path, LineReader, Pipe}, proto};
use std::{sync::mpsc, time::Duration};

#[test]
fn event_reader_handles_a_buffered_command_and_stops_while_final_status_flushes() {
    let path = random_pipe_path();
    let server = Pipe::create_server(&path).unwrap();
    let (sent, written) = mpsc::channel();
    let (answered, answer) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        server.accept().unwrap();
        let session = format!("{{\"type\":\"session\",\"v\":1,\"region\":\"US\",\"lang\":\"US\",\"start_game\":false,\"native_auth\":{{\"pol_id\":\"{}\",\"pol_password\":\"{}\",\"se_id\":\"{}\",\"se_secret\":\"{}\",\"otp\":null,\"profile_key\":\"{}\"}}}}\n",
            b64::encode(b"ABCD1234"), b64::encode(b"fake password"), b64::encode(b"fakeuser"), b64::encode(&[0x51; 20]), "a".repeat(40));
        server.write_all(format!("{session}{{\"type\":\"native_sign_out\"}}\n").as_bytes()).unwrap();
        sent.send(()).unwrap();
        let mut replies = LineReader::default();
        assert_eq!(replies.read_line(&server, Some(Duration::from_secs(2))).unwrap(),
            b"{\"type\":\"native_control_result\",\"action\":\"sign_out\",\"code\":0}");
        answered.send(()).unwrap();
        assert_eq!(replies.read_line(&server, Some(Duration::from_secs(2))).unwrap(), b"{\"type\":\"exit\",\"code\":0}");
    });
    let client = Pipe::connect(&path, std::process::id(), Duration::from_secs(5)).unwrap();
    written.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut reader = LineReader::default();
    assert!(matches!(reader.read_parsed(&client, Some(Duration::from_secs(2)), proto::parse_first_line).unwrap().unwrap(), proto::FirstLine::Session(_)));
    events::start(client, reader, true);
    answer.recv_timeout(Duration::from_secs(2)).unwrap();
    events::send(proto::exit_line(0));
    events::finish(Duration::from_secs(2));
    writer.join().unwrap();
}
