//! Continuous auto-detect sideband on the RDPEMT tunnel.
//!
//! After the connection sequence the server measures the multitransport link
//! by piggy-backing auto-detect requests on the sub-headers of Tunnel Data
//! PDUs ([MS-RDPEMT] 2.2.1.1.1, [MS-RDPBCGR] 2.2.14). Responses ride back the
//! same way, in a Tunnel Data PDU of their own. Without them the server keeps
//! whatever bandwidth estimate it had when the tunnel came up.

use std::time::Instant;

use ironrdp_pdu::rdp::autodetect::{
    AutoDetectRequest, AutoDetectResponse, BW_RESULTS_CONTINUOUS, BW_START_LOSSY_UDP, BW_START_RELIABLE_UDP,
    BW_STOP_LOSSY_UDP, BW_STOP_RELIABLE_UDP,
};
use ironrdp_rdpemt::{SubHeaderType, TunnelSubHeader};
use tracing::debug;

/// Sans-I/O responder for the tunnel's auto-detect sub-headers.
#[derive(Debug, Default)]
pub(crate) struct TunnelAutoDetect {
    /// Open Bandwidth Measure window: start time and wire bytes received since.
    window: Option<(Instant, u64)>,
}

impl TunnelAutoDetect {
    /// Account for one inbound Tunnel Data PDU of `pdu_len` wire bytes and
    /// answer the auto-detect requests carried in its `sub_headers`.
    ///
    /// Returns the response sub-headers to send back; empty when nothing is due.
    pub(crate) fn on_pdu(&mut self, pdu_len: usize, sub_headers: &[TunnelSubHeader]) -> Vec<TunnelSubHeader> {
        // Count first, so the PDU carrying Stop is inside the window and the one
        // carrying Start (which resets the count below) is not. After the
        // connection sequence every server-to-client PDU replaces the payload
        // messages ([MS-RDPBCGR] 2.2.14.2.2), so whole PDUs are what count.
        if let Some((_, bytes)) = self.window.as_mut() {
            *bytes = bytes.saturating_add(u64::try_from(pdu_len).unwrap_or(u64::MAX));
        }

        let mut responses = Vec::new();
        for sub in sub_headers {
            if sub.sub_header_type != SubHeaderType::AutoDetectRequest {
                continue;
            }
            let request = match ironrdp_core::decode::<AutoDetectRequest>(&sub.data) {
                Ok(request) => request,
                Err(error) => {
                    debug!(%error, "Undecodable auto-detect request in tunnel sub-header, ignoring");
                    continue;
                }
            };
            let Some(response) = self.handle(request) else {
                continue;
            };
            match ironrdp_core::encode_vec(&response) {
                Ok(data) => responses.push(TunnelSubHeader {
                    sub_header_type: SubHeaderType::AutoDetectResponse,
                    data,
                }),
                Err(error) => debug!(%error, "Failed to encode auto-detect response sub-header"),
            }
        }
        responses
    }

    fn handle(&mut self, request: AutoDetectRequest) -> Option<AutoDetectResponse> {
        match request {
            AutoDetectRequest::RttRequest { sequence_number, .. } => {
                debug!(sequence_number, "Answering tunnel RTT request");
                Some(AutoDetectResponse::RttResponse { sequence_number })
            }
            AutoDetectRequest::BandwidthMeasureStart {
                sequence_number,
                request_type,
            } if request_type == BW_START_RELIABLE_UDP || request_type == BW_START_LOSSY_UDP => {
                self.window = Some((Instant::now(), 0));
                debug!(sequence_number, "Tunnel bandwidth measurement started");
                None
            }
            // A Stop without an open window has nothing truthful to report; a zero
            // byte count would read as a dead link, so it is dropped instead.
            AutoDetectRequest::BandwidthMeasureStop {
                sequence_number,
                request_type,
                ..
            } if request_type == BW_STOP_RELIABLE_UDP || request_type == BW_STOP_LOSSY_UDP => {
                let Some((started_at, bytes)) = self.window.take() else {
                    debug!(
                        sequence_number,
                        "Tunnel Bandwidth Measure Stop without a Start, ignoring"
                    );
                    return None;
                };
                let time_delta_ms = u32::try_from(started_at.elapsed().as_millis())
                    .unwrap_or(u32::MAX)
                    .max(1);
                let byte_count = u32::try_from(bytes).unwrap_or(u32::MAX);
                debug!(
                    sequence_number,
                    time_delta_ms, byte_count, "Answering tunnel bandwidth measurement"
                );
                Some(AutoDetectResponse::BandwidthMeasureResults {
                    sequence_number,
                    response_type: BW_RESULTS_CONTINUOUS,
                    time_delta_ms,
                    byte_count,
                })
            }
            other => {
                debug!(request = ?other, "Unhandled auto-detect request in tunnel sub-header");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(request: AutoDetectRequest) -> TunnelSubHeader {
        TunnelSubHeader {
            sub_header_type: SubHeaderType::AutoDetectRequest,
            data: ironrdp_core::encode_vec(&request).expect("encode request"),
        }
    }

    fn decode_response(sub: &TunnelSubHeader) -> AutoDetectResponse {
        assert_eq!(sub.sub_header_type, SubHeaderType::AutoDetectResponse);
        ironrdp_core::decode::<AutoDetectResponse>(&sub.data).expect("decode response")
    }

    #[test]
    fn bandwidth_window_counts_whole_pdus_between_start_and_stop() {
        let mut autodetect = TunnelAutoDetect::default();

        // Traffic before Start is not counted; the Start PDU itself neither.
        assert!(autodetect.on_pdu(5000, &[]).is_empty());
        assert!(
            autodetect
                .on_pdu(12, &[request(AutoDetectRequest::bw_start_continuous(1))])
                .is_empty()
        );

        assert!(autodetect.on_pdu(1000, &[]).is_empty());
        assert!(autodetect.on_pdu(200, &[]).is_empty());

        // The Stop PDU is inside the window.
        let responses = autodetect.on_pdu(34, &[request(AutoDetectRequest::bw_stop_continuous(2))]);
        let [response] = responses.as_slice() else {
            panic!("expected one response sub-header, got {}", responses.len());
        };
        let AutoDetectResponse::BandwidthMeasureResults {
            sequence_number,
            response_type,
            time_delta_ms,
            byte_count,
        } = decode_response(response)
        else {
            panic!("expected Bandwidth Measure Results");
        };
        assert_eq!(sequence_number, 2);
        assert_eq!(response_type, BW_RESULTS_CONTINUOUS);
        assert_eq!(byte_count, 1234);
        assert!(time_delta_ms >= 1);

        // Closed window: a second Stop reports nothing.
        assert!(
            autodetect
                .on_pdu(10, &[request(AutoDetectRequest::bw_stop_continuous(3))])
                .is_empty()
        );
    }

    #[test]
    fn lossy_variants_open_and_close_the_window_too() {
        let mut autodetect = TunnelAutoDetect::default();
        let start = AutoDetectRequest::BandwidthMeasureStart {
            sequence_number: 1,
            request_type: BW_START_LOSSY_UDP,
        };
        let stop = AutoDetectRequest::BandwidthMeasureStop {
            sequence_number: 2,
            request_type: BW_STOP_LOSSY_UDP,
            payload: None,
        };
        assert!(autodetect.on_pdu(6, &[request(start)]).is_empty());
        let responses = autodetect.on_pdu(6, &[request(stop)]);
        assert!(matches!(
            decode_response(&responses[0]),
            AutoDetectResponse::BandwidthMeasureResults { byte_count: 6, .. }
        ));
    }

    #[test]
    fn rtt_requests_are_answered_and_other_sub_headers_ignored() {
        let mut autodetect = TunnelAutoDetect::default();
        let unrelated = TunnelSubHeader {
            sub_header_type: SubHeaderType::AutoDetectResponse,
            data: vec![0xFF],
        };
        let responses = autodetect.on_pdu(6, &[unrelated, request(AutoDetectRequest::rtt_continuous(9))]);
        assert_eq!(responses.len(), 1);
        assert_eq!(
            decode_response(&responses[0]),
            AutoDetectResponse::RttResponse { sequence_number: 9 }
        );
    }

    #[test]
    fn garbage_request_is_skipped() {
        let mut autodetect = TunnelAutoDetect::default();
        let garbage = TunnelSubHeader {
            sub_header_type: SubHeaderType::AutoDetectRequest,
            data: vec![0x06, 0x01, 0x00],
        };
        assert!(autodetect.on_pdu(6, &[garbage]).is_empty());
    }
}
