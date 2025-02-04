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

use std::iter::Iterator;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use ahash::AHashMap;
use arc_swap::ArcSwapOption;
use ip_network_table::IpNetworkTable;
use radix_trie::Trie;

use g3_types::metrics::{MetricsName, StaticMetricsTags};
use g3_types::net::{Host, UpstreamAddr};
use g3_types::resolve::ResolveStrategy;

use super::stats::{UserSiteDurationRecorder, UserSiteStats};
use super::{UserSiteDurationStats, UserType};
use crate::config::auth::UserSiteConfig;

struct DurationValue {
    recorder: Arc<UserSiteDurationRecorder>,
    // we have to keep a reference here, or it will be dropped in metrics
    _stats: Arc<UserSiteDurationStats>,
}

pub(super) struct UserSite {
    config: Arc<UserSiteConfig>,
    stats: Arc<UserSiteStats>,
    duration_recorder: Arc<Mutex<AHashMap<String, DurationValue>>>,
}

impl UserSite {
    fn new(config: &Arc<UserSiteConfig>, user: &str, user_group: &MetricsName) -> Self {
        UserSite {
            config: Arc::clone(config),
            stats: Arc::new(UserSiteStats::new(user, user_group, &config.id)),
            duration_recorder: Arc::new(Mutex::new(AHashMap::new())),
        }
    }

    fn new_for_reload(&self, config: &Arc<UserSiteConfig>) -> Self {
        if self.config.duration_stats != config.duration_stats {
            UserSite {
                config: Arc::clone(config),
                stats: self.stats.clone(),
                duration_recorder: Arc::new(Mutex::new(AHashMap::new())),
            }
        } else {
            UserSite {
                config: Arc::clone(config),
                stats: self.stats.clone(),
                duration_recorder: self.duration_recorder.clone(),
            }
        }
    }

    #[inline]
    pub(super) fn emit_stats(&self) -> bool {
        self.config.emit_stats
    }

    #[inline]
    pub(super) fn stats(&self) -> &Arc<UserSiteStats> {
        &self.stats
    }

    #[inline]
    pub(super) fn resolve_strategy(&self) -> Option<ResolveStrategy> {
        self.config.resolve_strategy
    }

    pub(crate) fn fetch_duration_recorder(
        &self,
        user_type: UserType,
        server: &MetricsName,
        server_extra_tags: &Arc<ArcSwapOption<StaticMetricsTags>>,
    ) -> Arc<UserSiteDurationRecorder> {
        let mut new_stats = None;

        let mut map = self.duration_recorder.lock().unwrap();
        let recorder = map
            .entry(server.to_string())
            .or_insert_with(|| {
                let (recorder, stats) = UserSiteDurationRecorder::new(
                    self.stats.user_group(),
                    self.stats.user(),
                    user_type,
                    server,
                    server_extra_tags,
                    &self.config.duration_stats,
                );
                new_stats = Some(stats.clone());
                DurationValue {
                    recorder: Arc::new(recorder),
                    _stats: stats,
                }
            })
            .recorder
            .clone();
        drop(map);

        if let Some(stats) = new_stats {
            crate::stat::user_site::push_duration_stats(stats, &self.config.id);
        }

        recorder
    }
}

#[derive(Default)]
pub(super) struct UserSites {
    all_sites: AHashMap<MetricsName, Arc<UserSite>>,
    exact_match_ipaddr: Option<AHashMap<IpAddr, Arc<UserSite>>>,
    exact_match_domain: Option<AHashMap<String, Arc<UserSite>>>,
    child_match_domain: Option<Trie<String, Arc<UserSite>>>,
    subnet_match_ipaddr: Option<IpNetworkTable<Arc<UserSite>>>,
}

impl UserSites {
    fn build<'a, T: Iterator<Item = &'a Arc<UserSiteConfig>>, F>(
        sites: T,
        build_user_site: F,
    ) -> Self
    where
        F: Fn(&Arc<UserSiteConfig>) -> UserSite,
    {
        let mut all_sites = AHashMap::new();
        let mut exact_match_ipaddr = AHashMap::new();
        let mut exact_match_domain = AHashMap::new();
        let mut child_match_domain = Trie::new();
        let mut child_match_domain_count = 0usize;
        let mut subnet_match_ipaddr = IpNetworkTable::new();

        for site_config in sites {
            let site = Arc::new(build_user_site(site_config));

            all_sites.insert(site_config.id.clone(), site.clone());
            for ip in &site_config.exact_match_ipaddr {
                exact_match_ipaddr.insert(*ip, site.clone());
            }
            for domain in &site_config.exact_match_domain {
                exact_match_domain.insert(domain.to_string(), site.clone());
            }
            for domain in &site_config.child_match_domain {
                let domain = g3_types::resolve::reverse_idna_domain(domain);
                if child_match_domain.insert(domain, site.clone()).is_none() {
                    child_match_domain_count += 1;
                }
            }
            for net in &site_config.subnet_match_ipaddr {
                subnet_match_ipaddr.insert(*net, site.clone());
            }
        }

        let exact_match_ipaddr = if exact_match_ipaddr.is_empty() {
            None
        } else {
            Some(exact_match_ipaddr)
        };
        let exact_match_domain = if exact_match_domain.is_empty() {
            None
        } else {
            Some(exact_match_domain)
        };
        let child_match_domain = if child_match_domain_count > 0 {
            Some(child_match_domain)
        } else {
            None
        };
        let subnet_match_ipaddr = if subnet_match_ipaddr.is_empty() {
            None
        } else {
            Some(subnet_match_ipaddr)
        };

        UserSites {
            all_sites,
            exact_match_ipaddr,
            exact_match_domain,
            child_match_domain,
            subnet_match_ipaddr,
        }
    }

    pub(super) fn new<'a, T: Iterator<Item = &'a Arc<UserSiteConfig>>>(
        sites: T,
        user: &str,
        user_group: &MetricsName,
    ) -> Self {
        Self::build(sites, |site_config| {
            UserSite::new(site_config, user, user_group)
        })
    }

    pub(super) fn new_for_reload<'a, T: Iterator<Item = &'a Arc<UserSiteConfig>>>(
        &self,
        sites: T,
        user: &str,
        user_group: &MetricsName,
    ) -> Self {
        Self::build(sites, |site_config| {
            if let Some(old) = self.all_sites.get(&site_config.id) {
                old.new_for_reload(site_config)
            } else {
                UserSite::new(site_config, user, user_group)
            }
        })
    }

    pub(super) fn fetch_site(&self, ups: &UpstreamAddr) -> Option<Arc<UserSite>> {
        match ups.host() {
            Host::Ip(ip) => {
                if let Some(ht) = &self.exact_match_ipaddr {
                    if let Some(r) = ht.get(ip) {
                        return Some(r.clone());
                    }
                }

                if let Some(tb) = &self.subnet_match_ipaddr {
                    if let Some((_n, r)) = tb.longest_match(*ip) {
                        return Some(r.clone());
                    }
                }
            }
            Host::Domain(domain) => {
                if let Some(ht) = &self.exact_match_domain {
                    if let Some(r) = ht.get(domain) {
                        return Some(r.clone());
                    }
                }

                if let Some(trie) = &self.child_match_domain {
                    let key = g3_types::resolve::reverse_idna_domain(domain);
                    if let Some(r) = trie.get_ancestor_value(&key) {
                        return Some(r.clone());
                    }
                }
            }
        }

        None
    }
}
