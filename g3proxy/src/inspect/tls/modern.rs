/*
 * Copyright 2023 ByteDance and/or its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::str::FromStr;
use std::sync::Arc;

use anyhow::anyhow;
use tokio::io::{AsyncRead, AsyncWrite};

use g3_dpi::{Protocol, ProtocolInspector};
use g3_io_ext::{AggregatedIo, FlexBufReader, OnceBufReader};
use g3_openssl::SslConnector;
use g3_types::net::{AlpnProtocol, Host};
use g3_udpdump::ExportedPduDissectorHint;

use super::{TlsInterceptIo, TlsInterceptObject, TlsInterceptionError};
use crate::config::server::ServerConfig;
use crate::inspect::{InterceptionError, StreamInspection};
use crate::log::inspect::{stream::StreamInspectLog, InspectSource};
use crate::serve::ServerTaskResult;

impl<SC> TlsInterceptObject<SC>
where
    SC: ServerConfig + Send + Sync + 'static,
{
    pub(crate) async fn intercept_modern(
        mut self,
        inspector: &mut ProtocolInspector,
    ) -> ServerTaskResult<StreamInspection<SC>> {
        match self.do_intercept_modern(inspector).await {
            Ok(obj) => {
                self.log_ok();
                Ok(obj)
            }
            Err(e) => {
                self.log_err(&e);
                Err(InterceptionError::Tls(e).into_server_task_error(Protocol::TlsModern))
            }
        }
    }

    async fn do_intercept_modern(
        &mut self,
        inspector: &mut ProtocolInspector,
    ) -> Result<StreamInspection<SC>, TlsInterceptionError> {
        let TlsInterceptIo {
            clt_r,
            clt_w,
            ups_r,
            ups_w,
        } = self.io.take().unwrap();

        let acceptor = rustls::server::Acceptor::default();
        let clt_io = AggregatedIo::new(clt_r, clt_w);

        let lazy_acceptor = tokio_rustls::LazyConfigAcceptor::new(acceptor, clt_io);

        // also use upstream timeout config for client handshake
        let handshake_timeout = self.tls_interception.client_config.handshake_timeout;

        let client_handshake = tokio::time::timeout(handshake_timeout, lazy_acceptor)
            .await
            .map_err(|_| TlsInterceptionError::ClientHandshakeTimeout)?
            .map_err(|e| {
                TlsInterceptionError::ClientHandshakeFailed(anyhow!(
                    "read client hello msg failed: {e:?}"
                ))
            })?;
        let client_hello = client_handshake.client_hello();

        // build to server ssl context based on client hello
        let sni_hostname = client_hello.server_name();
        if let Some(domain) = sni_hostname {
            if let Ok(host) = Host::from_str(domain) {
                self.upstream.set_host(host);
            }
        }
        let ups_ssl = self
            .tls_interception
            .client_config
            .build_ssl(sni_hostname, &self.upstream, client_hello.alpn())
            .map_err(|e| {
                TlsInterceptionError::UpstreamPrepareFailed(anyhow!(
                    "failed to build ssl context: {e}"
                ))
            })?;

        // fetch fake server cert early in the background
        let tls_interception = self.tls_interception.clone();
        let cert_domain = sni_hostname
            .map(|v| v.to_string())
            .unwrap_or_else(|| self.upstream.host().to_string());
        let clt_cert_handle =
            tokio::spawn(async move { tls_interception.cert_agent.fetch(cert_domain).await });

        // handshake with upstream server
        let ups_tls_connector = SslConnector::new(ups_ssl, AggregatedIo::new(ups_r, ups_w))
            .map_err(|e| {
                TlsInterceptionError::UpstreamPrepareFailed(anyhow!(
                    "failed to get ssl stream: {e}"
                ))
            })?;
        let ups_tls_stream = tokio::time::timeout(handshake_timeout, ups_tls_connector.connect())
            .await
            .map_err(|_| TlsInterceptionError::UpstreamHandshakeTimeout)?
            .map_err(|e| {
                TlsInterceptionError::UpstreamHandshakeFailed(anyhow!(
                    "upstream handshake error: {e}"
                ))
            })?;

        let ups_ssl = ups_tls_stream.ssl();
        let selected_alpn_protocol = ups_ssl.selected_alpn_protocol();

        // fetch fake server cert
        let (clt_cert, clt_key) = clt_cert_handle
            .await
            .map_err(|e| {
                TlsInterceptionError::NoFakeCertGenerated(anyhow!(
                    "join client cert handle failed: {e}"
                ))
            })?
            .ok_or_else(|| {
                TlsInterceptionError::NoFakeCertGenerated(anyhow!(
                    "failed to get fake upstream certificate"
                ))
            })?;

        // build to client ssl context based on server response, and handshake
        let mut clt_server_config = rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(clt_cert, clt_key)
            .map_err(|e| {
                TlsInterceptionError::ClientHandshakeFailed(anyhow!(
                    "failed to build client tls config: {e:?}"
                ))
            })?;
        let mut protocol = Protocol::Unknown;
        let has_alpn = if let Some(alpn_protocol) = selected_alpn_protocol {
            if let Some(p) = AlpnProtocol::from_buf(alpn_protocol) {
                inspector.push_alpn_protocol(p);
                protocol = Protocol::from(p);
            }

            clt_server_config.alpn_protocols = vec![alpn_protocol.to_owned()];
            true
        } else {
            false
        };
        let clt_tls_stream = tokio::time::timeout(
            handshake_timeout,
            client_handshake.into_stream(Arc::new(clt_server_config)),
        )
        .await
        .map_err(|_| TlsInterceptionError::ClientHandshakeTimeout)?
        .map_err(|e| {
            TlsInterceptionError::ClientHandshakeFailed(anyhow!("client handshake error: {e:?}"))
        })?;

        let (clt_r, clt_w) = tokio::io::split(clt_tls_stream);
        let (ups_r, ups_w) = tokio::io::split(ups_tls_stream);

        let obj = if let Some(stream_dumper) = self
            .tls_interception
            .get_stream_dumper(self.ctx.task_notes.worker_id)
        {
            let dissector_hint = if !protocol.wireshark_dissector().is_empty() {
                ExportedPduDissectorHint::Protocol(protocol)
            } else {
                ExportedPduDissectorHint::TlsPort(self.upstream.port())
            };
            let (clt_w, ups_w) = stream_dumper.wrap_io(
                self.ctx.task_notes.client_addr,
                self.ctx.task_notes.server_addr,
                dissector_hint,
                clt_w,
                ups_w,
            );
            self.inspect_inner(protocol, has_alpn, clt_r, clt_w, ups_r, ups_w)
        } else {
            self.inspect_inner(protocol, has_alpn, clt_r, clt_w, ups_r, ups_w)
        };
        Ok(obj)
    }

    fn inspect_inner<CR, CW, UR, UW>(
        &self,
        protocol: Protocol,
        has_alpn: bool,
        clt_r: CR,
        clt_w: CW,
        ups_r: UR,
        ups_w: UW,
    ) -> StreamInspection<SC>
    where
        CR: AsyncRead + Send + Unpin + 'static,
        CW: AsyncWrite + Send + Unpin + 'static,
        UR: AsyncRead + Send + Unpin + 'static,
        UW: AsyncWrite + Send + Unpin + 'static,
    {
        let mut ctx = self.ctx.clone();
        ctx.increase_inspection_depth();
        StreamInspectLog::new(&ctx).log(InspectSource::TlsAlpn, protocol);
        match protocol {
            Protocol::Http1 => {
                let mut h1_obj = crate::inspect::http::H1InterceptObject::new(ctx);
                h1_obj.set_io(
                    FlexBufReader::new(Box::new(clt_r)),
                    Box::new(clt_w),
                    Box::new(ups_r),
                    Box::new(ups_w),
                );
                StreamInspection::H1(h1_obj)
            }
            Protocol::Http2 => {
                let mut h2_obj = crate::inspect::http::H2InterceptObject::new(ctx);
                h2_obj.set_io(
                    OnceBufReader::with_no_buf(Box::new(clt_r)),
                    Box::new(clt_w),
                    Box::new(ups_r),
                    Box::new(ups_w),
                );
                StreamInspection::H2(h2_obj)
            }
            _ => {
                let mut stream_obj =
                    crate::inspect::stream::StreamInspectObject::new(ctx, self.upstream.clone());
                stream_obj.set_io(
                    Box::new(clt_r),
                    Box::new(clt_w),
                    Box::new(ups_r),
                    Box::new(ups_w),
                );
                if has_alpn {
                    // Just treat it as unknown. Unknown protocol should be forbidden if needed.
                    StreamInspection::StreamUnknown(stream_obj)
                } else {
                    // Inspect if no ALPN is set
                    StreamInspection::StreamInspect(stream_obj)
                }
            }
        }
    }
}
