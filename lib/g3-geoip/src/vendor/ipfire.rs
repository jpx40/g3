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

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::mem;
use std::path::Path;
use std::str::FromStr;

use anyhow::{anyhow, Context};
use flate2::bufread::GzDecoder;
use ip_network::IpNetwork;
use ip_network_table::IpNetworkTable;

use crate::{GeoIpAsnRecord, GeoIpCountryRecord, IsoCountryCode};

pub fn load_location(
    file: &Path,
) -> anyhow::Result<(
    IpNetworkTable<GeoIpCountryRecord>,
    IpNetworkTable<GeoIpAsnRecord>,
)> {
    if let Some(ext) = file.extension() {
        match ext.to_str() {
            Some("gz") => {
                let f = File::open(file)
                    .map_err(|e| anyhow!("failed to open gzip file {}: {e}", file.display()))?;
                let f = GzDecoder::new(BufReader::new(f));
                return load_location_from_dump(f).context(format!(
                    "failed to load records from gzip file {}",
                    file.display()
                ));
            }
            Some(_) => {}
            None => {}
        }
    }
    let f = File::open(file)
        .map_err(|e| anyhow!("failed to open dump file {}: {e}", file.display()))?;
    load_location_from_dump(f).context(format!(
        "failed to load records from dump file {}",
        file.display()
    ))
}

/// load ipfire location dump data (generated by `location dump` command)
fn load_location_from_dump<R: io::Read>(
    stream: R,
) -> anyhow::Result<(
    IpNetworkTable<GeoIpCountryRecord>,
    IpNetworkTable<GeoIpAsnRecord>,
)> {
    let mut as_name_table: HashMap<u32, String> = HashMap::new();
    let mut country_table = IpNetworkTable::new();
    let mut asn_table = IpNetworkTable::new();

    let reader = BufReader::new(stream);
    let mut block = Block::default();
    for (i, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| anyhow!("failed to read line {i}: {e}"))?;
        if line.starts_with('#') {
            continue;
        }

        if line.is_empty() {
            if let Some(net) = block.network.take() {
                if let Some(country) = block.country.take() {
                    country_table.insert(
                        net,
                        GeoIpCountryRecord {
                            network: net,
                            country,
                            continent: country.continent(),
                        },
                    );
                }
                if let Some(asn) = block.as_number.take() {
                    asn_table.insert(
                        net,
                        GeoIpAsnRecord {
                            network: net,
                            number: asn,
                            name: as_name_table.get(&asn).cloned(),
                            domain: None,
                        },
                    );
                }
            } else if let Some(asn) = block.as_number.take() {
                if !block.as_name.is_empty() {
                    as_name_table.insert(asn, mem::take(&mut block.as_name));
                }
            }
            continue;
        }

        if let Some((key, value)) = line.split_once(':') {
            block.set(key, value.trim());
        }
    }

    Ok((country_table, asn_table))
}

#[derive(Default)]
struct Block {
    as_number: Option<u32>,
    as_name: String,
    network: Option<IpNetwork>,
    country: Option<IsoCountryCode>,
}

impl Block {
    fn set(&mut self, key: &str, value: &str) {
        match key {
            "aut-num" => {
                self.as_number = u32::from_str(value.strip_prefix("AS").unwrap_or(value)).ok()
            }
            "name" => self.as_name = value.to_string(),
            "net" => self.network = IpNetwork::from_str(value).ok(),
            "country" => self.country = IsoCountryCode::from_str(value).ok(),
            _ => {}
        }
    }
}
