use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::timeout;

/// IPMI Power Control via UDP (for Intel AMT/vPro devices)
pub struct AmtController {
    host: String,
    port: u16,
    username: String,
    password: String,
}

impl AmtController {
    pub fn new(host: String, port: u16, username: String, password: String) -> Self {
        Self {
            host,
            port,
            username,
            password,
        }
    }

    /// Send power-on command to the AMT device via IPMI
    pub async fn power_on(&self) -> Result<String, String> {
        // Validate host
        let addr = format!("{}:{}", self.host, self.port);

        // Create a UDP socket
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| format!("Failed to create socket: {}", e))?;

        // Test connection first
        match socket.send_to(&[0], &addr).await {
            Ok(_) => {},
            Err(e) => {
                return Err(format!(
                    "Failed to connect to {}:{}: {}",
                    self.host, self.port, e
                ));
            }
        }

        // IPMI Open Session Request (simplified)
        // This creates an IPMI v2.0 session
        let open_session_req = build_open_session_request();

        socket
            .send_to(&open_session_req, &addr)
            .await
            .map_err(|e| format!("Failed to send open session request: {}", e))?;

        let mut buf = vec![0u8; 1024];
        let (n, _) = timeout(Duration::from_secs(5), socket.recv_from(&mut buf))
            .await
            .map_err(|_| "Open session response timeout".to_string())?
            .map_err(|e| format!("Failed to receive open session response: {}", e))?;

        let open_session_resp = &buf[..n];

        // Parse session ID and challenge token from response
        let (session_id, challenge_token) =
            parse_open_session_response(open_session_resp)
                .map_err(|e| format!("Failed to parse session response: {}", e))?;

        // IPMI RAKP1 (authentication) Request
        let rakp1_req =
            build_rakp1_request(&self.username, &challenge_token, session_id);

        socket
            .send_to(&rakp1_req, &addr)
            .await
            .map_err(|e| format!("Failed to send RAKP1 request: {}", e))?;

        let (n, _) = timeout(Duration::from_secs(5), socket.recv_from(&mut buf))
            .await
            .map_err(|_| "RAKP1 response timeout".to_string())?
            .map_err(|e| format!("Failed to receive RAKP1 response: {}", e))?;

        let rakp1_resp = &buf[..n];

        // Parse RAKP1 response
        let server_challenge =
            parse_rakp1_response(rakp1_resp)
                .map_err(|e| format!("Failed to parse RAKP1 response: {}", e))?;

        // IPMI RAKP3 (authentication) Request
        let rakp3_req = build_rakp3_request(&self.password, &server_challenge, session_id);

        socket
            .send_to(&rakp3_req, &addr)
            .await
            .map_err(|e| format!("Failed to send RAKP3 request: {}", e))?;

        let (n, _) = timeout(Duration::from_secs(5), socket.recv_from(&mut buf))
            .await
            .map_err(|_| "RAKP3 response timeout".to_string())?
            .map_err(|e| format!("Failed to receive RAKP3 response: {}", e))?;

        // Session should now be established - send power-on command
        let power_on_req = build_power_on_request(session_id);

        socket
            .send_to(&power_on_req, &addr)
            .await
            .map_err(|e| format!("Failed to send power-on command: {}", e))?;

        let (n, _) = timeout(Duration::from_secs(5), socket.recv_from(&mut buf))
            .await
            .map_err(|_| "Power-on response timeout".to_string())?
            .map_err(|e| format!("Failed to receive power-on response: {}", e))?;

        let power_on_resp = &buf[..n];

        // Check response for success
        if check_power_on_success(power_on_resp) {
            Ok("Successfully powered on".to_string())
        } else {
            Err("Power-on command failed - check device status".to_string())
        }
    }
}

// IPMI Protocol Builders

fn build_open_session_request() -> Vec<u8> {
    // IPMI Open Session Request (v2.0/IPMI 2.0)
    vec![
        0x06, 0x00, 0x01, 0x01, // Header + Open Session tag
        0x00, 0x00, 0x00, 0x00, // Reserved
        0x08, 0x04, 0x01, 0x00, // Authentication algorithm
        0x01, 0x00, 0x00, 0x08, // Integrity algorithm
        0x01, 0x00, 0x00, 0x00, // Confidentiality algorithm
    ]
}

fn parse_open_session_response(resp: &[u8]) -> Result<(u32, Vec<u8>), String> {
    if resp.len() < 20 {
        return Err("Invalid open session response".to_string());
    }

    // Extract session ID from response
    let session_id = u32::from_le_bytes([resp[8], resp[9], resp[10], resp[11]]);

    // Extract challenge token (server-generated challenge)
    let challenge_len = if resp.len() > 20 { resp[19] as usize } else { 0 };
    let challenge_token = if challenge_len > 0 && resp.len() >= 20 + challenge_len {
        resp[20..20 + challenge_len].to_vec()
    } else {
        vec![0; 16]
    };

    Ok((session_id, challenge_token))
}

fn build_rakp1_request(username: &str, challenge_token: &[u8], session_id: u32) -> Vec<u8> {
    let mut req = Vec::new();

    // Header
    req.push(0x06);
    req.push(0x00);
    req.push(0x02);
    req.push(0x02); // RAKP1

    // Session ID
    req.extend_from_slice(&session_id.to_le_bytes());

    // Challenge token
    if !challenge_token.is_empty() {
        req.extend_from_slice(challenge_token);
    } else {
        req.extend_from_slice(&[0u8; 16]);
    }

    // Privilege level and username
    req.push(0x04); // Admin privilege
    req.push(0x00); // Reserved
    req.push(username.len() as u8);
    req.extend_from_slice(username.as_bytes());

    req
}

fn parse_rakp1_response(resp: &[u8]) -> Result<Vec<u8>, String> {
    if resp.len() < 20 {
        return Err("Invalid RAKP1 response".to_string());
    }

    // Extract server challenge from response
    let challenge_offset = 20;
    let challenge_len = if resp.len() > challenge_offset {
        16.min(resp.len() - challenge_offset)
    } else {
        16
    };

    Ok(resp[challenge_offset..challenge_offset + challenge_len].to_vec())
}

fn build_rakp3_request(password: &str, server_challenge: &[u8], session_id: u32) -> Vec<u8> {
    let mut req = Vec::new();

    // Header
    req.push(0x06);
    req.push(0x00);
    req.push(0x03);
    req.push(0x03); // RAKP3

    // Session ID
    req.extend_from_slice(&session_id.to_le_bytes());

    // Server challenge hash (simplified - use password directly for demo)
    // In production, this should be HMAC-SHA1 of challenge + password
    req.extend_from_slice(server_challenge);

    // Password (simplified)
    req.push(password.len() as u8);
    req.extend_from_slice(password.as_bytes());

    req
}

fn build_power_on_request(session_id: u32) -> Vec<u8> {
    vec![
        0x06, 0x00, 0x01, 0x01, // Header
        session_id as u8,
        (session_id >> 8) as u8,
        (session_id >> 16) as u8,
        (session_id >> 24) as u8, // Session ID
        0x01,
        0x01,
        0x00,
        0x0c, // Chassis power up command
        0x01,
        0x00,
        0x00,
        0x00,
    ]
}

fn check_power_on_success(resp: &[u8]) -> bool {
    // Check if response indicates success
    // Status code 0 = success
    if resp.is_empty() {
        return false;
    }

    // Simple check for completion code
    if resp.len() > 8 {
        return resp[8] == 0x00;
    }

    true
}
