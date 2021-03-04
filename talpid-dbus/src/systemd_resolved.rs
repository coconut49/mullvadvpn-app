use dbus::{
    self,
    arg::{self, RefArg},
    blocking::{
        stdintf::org_freedesktop_dbus::{Properties, PropertiesPropertiesChanged},
        Proxy, SyncConnection,
    },
    message::{MatchRule, SignalArgs},
};
use lazy_static::lazy_static;
use libc::{AF_INET, AF_INET6};
use std::{fs, io, net::IpAddr, path::Path, sync::Arc, time::Duration};

pub type Result<T> = std::result::Result<T, Error>;


#[derive(err_derive::Error, Debug)]
#[error(no_from)]
pub enum Error {
    #[error(display = "Failed to initialize a connection to D-Bus")]
    ConnectDBus(#[error(source)] dbus::Error),

    #[error(display = "Failed to read /etc/resolv.conf: _0")]
    ReadResolvConfError(#[error(source)] io::Error),

    #[error(display = "/etc/resolv.conf contents do not match systemd-resolved resolv.conf")]
    ResolvConfDiffers,

    #[error(display = "/etc/resolv.conf is not a symlink to Systemd resolved")]
    NotSymlinkedToResolvConf,

    #[error(display = "Static stub file does not point to localhost")]
    StaticStubNotPointingToLocalhost,

    #[error(display = "Systemd resolved not detected")]
    NoSystemdResolved(#[error(source)] dbus::Error),

    #[error(display = "Failed to find link interface in resolved manager")]
    GetLinkError(#[error(source)] Box<Error>),

    #[error(display = "Failed to configure DNS domains")]
    SetDomainsError(#[error(source)] dbus::Error),

    #[error(display = "Failed to revert DNS settings of interface: {}", _0)]
    RevertDnsError(String, #[error(source)] dbus::Error),

    #[error(display = "Failed to perform RPC call on D-Bus")]
    DBusRpcError(#[error(source)] dbus::Error),

    #[error(display = "Failed to add a match to listen for DNS config updates")]
    DnsUpdateMatchError(#[error(source)] dbus::Error),

    #[error(display = "Failed to remove a match for DNS config updates")]
    DnsUpdateRemoveMatchError(#[error(source)] dbus::Error),
}

lazy_static! {
    static ref RESOLVED_STUB_PATHS: Vec<&'static Path> = vec![
        Path::new("/run/systemd/resolve/stub-resolv.conf"),
        Path::new("/run/systemd/resolve/resolv.conf"),
        Path::new("/var/run/systemd/resolve/stub-resolv.conf"),
        Path::new("/var/run/systemd/resolve/resolv.conf"),
    ];
}

const RESOLV_CONF_PATH: &str = "/etc/resolv.conf";
const STATIC_STUB_PATH: &str = "/usr/lib/systemd/resolv.conf";

const RESOLVED_BUS: &str = "org.freedesktop.resolve1";
const RESOLVED_MANAGER_PATH: &str = "/org/freedesktop/resolve1";

const RPC_TIMEOUT: Duration = Duration::from_secs(1);

const LINK_INTERFACE: &str = "org.freedesktop.resolve1.Link";
const MANAGER_INTERFACE: &str = "org.freedesktop.resolve1.Manager";
const DNS_SERVERS: &str = "DNS";
const GET_LINK_METHOD: &str = "GetLink";
const SET_DNS_METHOD: &str = "SetDNS";
const SET_DOMAINS_METHOD: &str = "SetDomains";
const REVERT_METHOD: &str = "Revert";

#[derive(Clone)]
pub struct SystemdResolved {
    pub dbus_connection: Arc<SyncConnection>,
}

pub struct DnsState {
    pub interface_path: dbus::Path<'static>,
    pub interface_index: u32,
    pub set_servers: Vec<IpAddr>,
}

impl SystemdResolved {
    pub fn new() -> Result<Self> {
        let dbus_connection = crate::get_connection().map_err(Error::ConnectDBus)?;

        let systemd_resolved = SystemdResolved { dbus_connection };

        systemd_resolved.ensure_resolved_exists()?;
        Self::ensure_resolv_conf_is_resolved_symlink()?;
        Ok(systemd_resolved)
    }

    pub fn ensure_resolved_exists(&self) -> Result<()> {
        let _: Box<dyn RefArg> = self
            .as_manager_object()
            .get(&MANAGER_INTERFACE, "DNS")
            .map_err(Error::NoSystemdResolved)?;

        Ok(())
    }

    pub fn ensure_resolv_conf_is_resolved_symlink() -> Result<()> {
        match fs::read_link(RESOLV_CONF_PATH) {
            Ok(link_target) => {
                // if /etc/resolv.conf is not symlinked to the stub resolve.conf file , managing DNS
                // through systemd-resolved will not ensure that our resolver is given priority -
                // sometimes this will mean adding 1 and 2 seconds of latency to DNS
                // queries, other times our resolver won't be considered at all. In
                // this case, it's better to fall back to cruder management methods.
                if Self::path_is_resolvconf_stub(&link_target)
                    || Self::resolv_conf_is_static_stub(&link_target)?
                    || Self::ensure_resolvconf_contents().is_ok()
                {
                    Ok(())
                } else {
                    Err(Error::NotSymlinkedToResolvConf)
                }
            }
            // etc/resolv.conf is not a symlink
            Err(err) if err.kind() == io::ErrorKind::InvalidInput => {
                Self::ensure_resolvconf_contents()
            }
            Err(err) => {
                log::trace!("Failed to read /etc/resolv.conf symlink - {}", err);
                Err(Error::NotSymlinkedToResolvConf)
            }
        }
    }

    fn ensure_resolvconf_contents() -> Result<()> {
        let resolv_conf =
            fs::read_to_string(RESOLV_CONF_PATH).map_err(Error::ReadResolvConfError)?;
        if RESOLVED_STUB_PATHS
            .iter()
            .filter_map(|path| fs::read_to_string(path).ok())
            .any(|link_contents| link_contents == resolv_conf)
        {
            Ok(())
        } else {
            Err(Error::ResolvConfDiffers)
        }
    }

    fn path_is_resolvconf_stub(link_path: &Path) -> bool {
        // if link path is relative to /etc/resolv.conf, resolve the path and compare it.
        if link_path.is_relative() {
            match Path::new("/etc/").join(link_path).canonicalize() {
                Ok(link_destination) => RESOLVED_STUB_PATHS.contains(&link_destination.as_ref()),
                Err(e) => {
                    log::error!(
                        "Failed to canonicalize resolv conf path {} - {}",
                        link_path.display(),
                        e
                    );
                    false
                }
            }
        } else {
            RESOLVED_STUB_PATHS.contains(&link_path)
        }
    }

    /// Checks if path is pointing to the systemd-resolved _static_ resolv.conf file. If it's not,
    /// it returns false, otherwise it checks whether the static stub file points to the local
    /// resolver. If not, the file has been _meddled_ with, so we can't trust it.
    fn resolv_conf_is_static_stub(link_path: &Path) -> Result<bool> {
        if link_path == &STATIC_STUB_PATH.as_ref() {
            let points_to_localhost = fs::read_to_string(link_path)
                .map(|contents| {
                    let parts = contents.trim().split(' ');
                    parts
                        .map(str::parse::<IpAddr>)
                        .map(|maybe_ip| maybe_ip.map(|addr| addr.is_loopback()).unwrap_or(false))
                        .any(|is_loopback| is_loopback)
                })
                .unwrap_or(false);

            if points_to_localhost {
                Ok(true)
            } else {
                Err(Error::StaticStubNotPointingToLocalhost)
            }
        } else {
            Ok(false)
        }
    }


    fn as_manager_object(&self) -> Proxy<'_, &SyncConnection> {
        Proxy::new(
            RESOLVED_BUS,
            "/org/freedesktop/resolve1",
            RPC_TIMEOUT,
            &self.dbus_connection,
        )
    }

    fn as_link_object<'a>(
        &'a self,
        link_object_path: dbus::Path<'a>,
    ) -> Proxy<'a, &'a SyncConnection> {
        Proxy::new(
            RESOLVED_BUS,
            link_object_path,
            RPC_TIMEOUT,
            &self.dbus_connection,
        )
    }

    pub fn set_dns(&self, interface_index: u32, servers: &[IpAddr]) -> Result<DnsState> {
        let link_object_path = self
            .fetch_link(interface_index)
            .map_err(|e| Error::GetLinkError(Box::new(e)))?;

        let mut set_servers = servers.to_vec();
        set_servers.sort();
        self.set_link_dns(&link_object_path, servers)?;
        Ok(DnsState {
            interface_path: link_object_path,
            interface_index,
            set_servers,
        })
    }

    fn fetch_link(&self, interface_index: u32) -> Result<dbus::Path<'static>> {
        self.as_manager_object()
            .method_call(
                MANAGER_INTERFACE,
                GET_LINK_METHOD,
                (interface_index as i32,),
            )
            .map_err(Error::DBusRpcError)
            .map(|result: (dbus::Path<'static>,)| result.0)
    }

    fn set_link_dns<'a, 'b: 'a>(
        &'a self,
        link_object_path: &'b dbus::Path<'static>,
        servers: &[IpAddr],
    ) -> Result<()> {
        let servers = servers
            .iter()
            .map(|addr| (ip_version(addr), ip_to_bytes(addr)))
            .collect::<Vec<_>>();
        self.as_link_object(link_object_path.clone())
            .method_call(LINK_INTERFACE, SET_DNS_METHOD, (servers,))
            .map_err(Error::DBusRpcError)?;

        // set the search domain to catch all DNS requests, forces the link to be the prefered
        // resolver, otherwise systemd-resolved will use other interfaces to do DNS lookups
        let dns_domains: &[_] = &[(&".", true)];

        Proxy::new(
            RESOLVED_BUS,
            link_object_path,
            RPC_TIMEOUT,
            &*self.dbus_connection,
        )
        .method_call(LINK_INTERFACE, SET_DOMAINS_METHOD, (dns_domains,))
        .map_err(Error::SetDomainsError)
    }

    pub fn revert_link(&mut self, dns_state: DnsState) -> std::result::Result<(), dbus::Error> {
        let link = self.as_link_object(dns_state.interface_path);

        if let Err(error) = link.method_call::<(), _, _, _>(LINK_INTERFACE, REVERT_METHOD, ()) {
            if error.name() == Some("org.freedesktop.DBus.Error.UnknownObject") {
                log::trace!(
                    "Not resetting DNS of interface {} because it no longer exists",
                    dns_state.interface_index
                );
                Ok(())
            } else {
                Err(error)
            }
        } else {
            Ok(())
        }
    }

    pub fn watch_dns_changes<F: FnMut(Vec<DnsServer>) + Send + Sync + 'static, S: Fn() -> bool>(
        &mut self,
        mut callback: F,
        should_continue: S,
    ) -> Result<()> {
        let mut match_rule =
            MatchRule::new_signal(PropertiesPropertiesChanged::INTERFACE, DNS_SERVERS);
        match_rule.member = None;
        match_rule.path = Some(RESOLVED_MANAGER_PATH.into());
        let dns_matcher = self
            .dbus_connection
            .add_match(
                match_rule,
                move |mut prop_changed: PropertiesPropertiesChanged, _connection, _message| {
                    if let Some(dns_change) = prop_changed
                        .changed_properties
                        .get_mut(DNS_SERVERS)
                        .and_then(|dns_change| {
                            dns_change.as_iter().and_then(|mut iter| iter.next())
                        })
                    {
                        match DnsServer::server_list_from_refarg(dns_change) {
                            Some(new_server_list) => {
                                callback(new_server_list);
                            }
                            None => {
                                log::error!("Failed to deserialize message {:?}", dns_change);
                            }
                        }
                    };
                    true
                },
            )
            .map_err(Error::DnsUpdateMatchError)?;

        while should_continue() {
            if let Err(err) = self.dbus_connection.process(RPC_TIMEOUT) {
                log::error!("Failed to process DBus messages: {}", err);
            }
        }

        self.dbus_connection
            .remove_match(dns_matcher)
            .map_err(Error::DnsUpdateRemoveMatchError)
    }
}

#[derive(Debug)]
pub struct DnsServer {
    pub iface_index: i32,
    pub address_family: i32,
    pub address: IpAddr,
}

impl DnsServer {
    fn from_refarg(refarg: &dyn RefArg) -> Option<Self> {
        let mut iter = refarg.as_iter()?;
        let iface_index = *arg::cast(&*iter.next()?.box_clone())?;
        let address_family = *arg::cast(&*iter.next()?.box_clone())?;

        let ip_bytes = iter.next()?.box_clone();
        let ip_bytes: &Vec<u8> = arg::cast(&ip_bytes)?;
        let address = ip_from_bytes(&ip_bytes)?;
        Some(Self {
            iface_index,
            address_family,
            address,
        })
    }

    fn server_list_from_refarg(refarg: &dyn RefArg) -> Option<Vec<Self>> {
        let iter = refarg.as_iter()?;
        Some(iter.filter_map(DnsServer::from_refarg).collect())
    }
}

fn ip_version(address: &IpAddr) -> i32 {
    match address {
        IpAddr::V4(_) => AF_INET,
        IpAddr::V6(_) => AF_INET6,
    }
}

fn ip_to_bytes(address: &IpAddr) -> Vec<u8> {
    match address {
        IpAddr::V4(v4_address) => v4_address.octets().to_vec(),
        IpAddr::V6(v6_address) => v6_address.octets().to_vec(),
    }
}

fn ip_from_bytes(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => {
            let mut ipv4_bytes = [0u8; 4];
            &mut ipv4_bytes.copy_from_slice(bytes);
            Some(IpAddr::from(ipv4_bytes))
        }
        16 => {
            let mut ipv6_bytes = [0u8; 16];
            &mut ipv6_bytes.copy_from_slice(bytes);
            Some(IpAddr::from(ipv6_bytes))
        }
        _ => None,
    }
}