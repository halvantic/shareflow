/// WS-Management (WSMAN) Power Control for Intel AMT
/// Uses HTTP Digest Auth to connect to Intel AMT port 16992
use reqwest::Client;
use std::time::Duration;

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

    /// Power on the AMT device via WS-Management with Digest Auth
    pub async fn power_on(&self) -> Result<String, String> {
        let url = format!("http://{}:{}/wsman", self.host, self.port);
        log::info!("AMT: Sending power-on to {}", url);

        let body = build_wsman_power_on_request(&url);

        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

        // First request to get auth challenge
        log::debug!("AMT: Sending initial request to get digest challenge");
        let response = client
            .post(&url)
            .header("Content-Type", "application/soap+xml;charset=UTF-8")
            .body(body.clone())
            .send()
            .await
            .map_err(|e| {
                log::error!("AMT: Connection failed: {}", e);
                format!("Failed to connect to {}: {}", url, e)
            })?;

        let status = response.status();

        // Handle 401 Unauthorized (digest challenge)
        if status.as_u16() == 401 {
            log::info!("AMT: Received 401 challenge, computing digest auth");

            // Extract WWW-Authenticate header
            let auth_header = match response.headers().get("www-authenticate") {
                Some(h) => h.to_str().unwrap_or(""),
                None => {
                    log::error!("AMT: No WWW-Authenticate header in 401 response");
                    return Err("Device returned 401 but no auth challenge".to_string());
                }
            };

            log::debug!("AMT: Auth challenge: {}", auth_header);

            // Parse digest challenge and compute response
            match compute_digest_auth(
                &self.username,
                &self.password,
                "POST",
                &url,
                auth_header,
            ) {
                Ok(auth_header) => {
                    log::debug!("AMT: Sending authenticated request");

                    // Second request with digest auth
                    let response = client
                        .post(&url)
                        .header("Content-Type", "application/soap+xml;charset=UTF-8")
                        .header("Authorization", auth_header)
                        .body(body)
                        .send()
                        .await
                        .map_err(|e| {
                            log::error!("AMT: Authenticated request failed: {}", e);
                            format!("Authenticated request failed: {}", e)
                        })?;

                    let status = response.status();
                    let body_text = response
                        .text()
                        .await
                        .unwrap_or_else(|_| "Unable to read response".to_string());

                    log::info!("AMT: Response status: {}", status);
                    log::debug!("AMT: Response: {}", &body_text[..body_text.len().min(300)]);

                    if status.is_success() {
                        log::info!("AMT: Power-on command successful");
                        Ok("Power-on command sent successfully".to_string())
                    } else {
                        log::error!("AMT: Request failed with status {}", status);
                        Err(format!("Request failed (status {})", status))
                    }
                }
                Err(e) => {
                    log::error!("AMT: Failed to compute digest auth: {}", e);
                    Err(format!("Authentication computation failed: {}", e))
                }
            }
        } else {
            // No auth required or different auth method
            let body_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read response".to_string());

            log::info!("AMT: Response status: {}", status);
            log::debug!("AMT: Response: {}", &body_text[..body_text.len().min(300)]);

            if status.is_success() {
                log::info!("AMT: Power-on successful");
                Ok("Power-on command sent successfully".to_string())
            } else {
                log::error!("AMT: Request failed with status {}", status);
                Err(format!(
                    "Request failed (status {}). Check credentials and device configuration.",
                    status
                ))
            }
        }
    }
}

/// Compute Digest Auth header for HTTP authentication
fn compute_digest_auth(
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    www_auth: &str,
) -> Result<String, String> {
    // Parse challenge from WWW-Authenticate header
    // Format: Digest realm="...", domain="...", nonce="...", opaque="...", algorithm=MD5, qop="auth"

    let realm = extract_digest_param(www_auth, "realm")
        .ok_or("Missing realm in auth challenge")?;
    let nonce = extract_digest_param(www_auth, "nonce")
        .ok_or("Missing nonce in auth challenge")?;
    let opaque = extract_digest_param(www_auth, "opaque");
    let qop = extract_digest_param(www_auth, "qop");

    // Extract uri path (without domain)
    let uri_path = uri.split("://").nth(1).and_then(|s| s.split("/").nth(1)).unwrap_or(uri);

    // Compute HA1: MD5(username:realm:password)
    let ha1_input = format!("{}:{}:{}", username, realm, password);
    let ha1 = format!("{:x}", md5::compute(ha1_input.as_bytes()));

    // Compute HA2: MD5(method:uri)
    let ha2_input = format!("{}:{}", method, uri_path);
    let ha2 = format!("{:x}", md5::compute(ha2_input.as_bytes()));

    // Compute response: MD5(HA1:nonce:HA2)
    // For qop=auth: MD5(HA1:nonce:nc:cnonce:qop:HA2)
    let response = if qop.as_deref() == Some("auth") {
        let nc = "00000001";
        let cnonce = "0a4f113b";
        let response_input = format!("{}:{}:{}:{}:auth:{}", ha1, nonce, nc, cnonce, ha2);
        format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{:x}\", opaque=\"{}\", qop=auth, nc={}, cnonce=\"{}\"",
            username, realm, nonce, uri_path,
            md5::compute(response_input.as_bytes()),
            opaque.unwrap_or_default(),
            nc, cnonce
        )
    } else {
        let response_input = format!("{}:{}:{}", ha1, nonce, ha2);
        format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{:x}\"{}",
            username, realm, nonce, uri_path,
            md5::compute(response_input.as_bytes()),
            if let Some(o) = opaque {
                format!(", opaque=\"{}\"", o)
            } else {
                String::new()
            }
        )
    };

    log::debug!("AMT: Computed digest auth header");
    Ok(response)
}

/// Extract parameter value from Digest auth challenge
fn extract_digest_param(auth_header: &str, param: &str) -> Option<String> {
    let pattern = format!("{}=\"", param);
    if let Some(start) = auth_header.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = auth_header[value_start..].find('"') {
            return Some(auth_header[value_start..value_start + end].to_string());
        }
    }
    None
}

fn build_wsman_power_on_request(url: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope
  xmlns:s="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wsman="http://schemas.dmtf.org/wbem/wsman/1/wsman.xsd"
  xmlns:pcs="http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_PowerManagementService">
  <s:Header>
    <wsa:Action>http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_PowerManagementService/RequestPowerStateChange</wsa:Action>
    <wsa:To>{}</wsa:To>
    <wsman:ResourceURI>http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_PowerManagementService</wsman:ResourceURI>
    <wsa:MessageID>1</wsa:MessageID>
    <wsa:ReplyTo>
      <wsa:Address>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsa:Address>
    </wsa:ReplyTo>
    <wsman:SelectorSet>
      <wsman:Selector Name="CreationClassName">CIM_PowerManagementService</wsman:Selector>
      <wsman:Selector Name="Name">Intel(r) AMT Power Management Service</wsman:Selector>
      <wsman:Selector Name="SystemCreationClassName">CIM_ComputerSystem</wsman:Selector>
      <wsman:Selector Name="SystemName">Intel(r) AMT</wsman:Selector>
    </wsman:SelectorSet>
  </s:Header>
  <s:Body>
    <pcs:RequestPowerStateChange_INPUT>
      <pcs:PowerState>2</pcs:PowerState>
      <pcs:ManagedElement>
        <wsa:Address>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsa:Address>
        <wsa:ReferenceParameters>
          <wsman:ResourceURI>http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_ComputerSystem</wsman:ResourceURI>
          <wsman:SelectorSet>
            <wsman:Selector Name="CreationClassName">CIM_ComputerSystem</wsman:Selector>
            <wsman:Selector Name="Name">ManagedSystem</wsman:Selector>
          </wsman:SelectorSet>
        </wsa:ReferenceParameters>
      </pcs:ManagedElement>
    </pcs:RequestPowerStateChange_INPUT>
  </s:Body>
</s:Envelope>"#,
        url
    )
}
