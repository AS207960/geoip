#[macro_use]
extern crate log;

use prost::Message;
use notify::Watcher;
use std::convert::TryInto;

mod proto {
    include!(concat!(env!("OUT_DIR"), "/geoip.rs"));
}

fn db_watcher_thread(mmdb_reader: std::sync::Arc<std::sync::Mutex<maxminddb::Reader<Vec<u8>>>>, watcher_rx: std::sync::mpsc::Receiver<notify::DebouncedEvent>) {
    info!("Watching DB for updates");
    loop {
        match watcher_rx.recv() {
            Ok(event) => {
                match event {
                    notify::DebouncedEvent::Write(path) => {
                        let new_mmdb_reader = maxminddb::Reader::open_readfile(path).unwrap();
                        *mmdb_reader.lock().unwrap() = new_mmdb_reader;
                        info!("DB updated");
                    },
                    _ => {}
                }
            },
            Err(e) => error!("DB watch error: {:?}", e)
        }
    }
}

fn main() {
    pretty_env_logger::init();

    let (watcher_tx, watcher_rx) = std::sync::mpsc::channel();
    let mut watcher = notify::watcher(watcher_tx, std::time::Duration::from_secs(10)).unwrap();

    let mmdb_reader = std::sync::Arc::new(std::sync::Mutex::new(
        maxminddb::Reader::open_readfile(std::env::var("GEOIP_DATA_FILE").unwrap()).unwrap()
    ));
    let watcher_mmdb_reader = mmdb_reader.clone();
    watcher.watch(std::env::var("GEOIP_DATA_FILE").unwrap(), notify::RecursiveMode::NonRecursive).unwrap();

    std::thread::spawn(|| {
        db_watcher_thread(watcher_mmdb_reader, watcher_rx)
    });

    let mut amqp_connection = amiquip::Connection::insecure_open(&std::env::var("RABBITMQ_RPC_URL").unwrap()).unwrap();
    let amqp_channel = amqp_connection.open_channel(None).unwrap();

    let rpc_queue = amqp_channel.queue_declare("geoip_rpc", amiquip::QueueDeclareOptions {
        durable: false,
        ..amiquip::QueueDeclareOptions::default()
    }).unwrap();
    let rpc_consumer = rpc_queue.consume(amiquip::ConsumerOptions {
        no_ack: false,
        ..amiquip::ConsumerOptions::default()
    }).unwrap();
    info!("RPC handler running");

    for message in rpc_consumer.receiver().iter() {
        match message {
            amiquip::ConsumerMessage::Delivery(delivery) => {
                let (reply_to, correlation_id) = match (delivery.properties.reply_to(), delivery.properties.correlation_id()) {
                    (Some(r), Some(c)) => (r.to_string(), c.to_string()),
                    _ => {
                        rpc_consumer.ack(delivery).unwrap();
                        continue;
                    }
                };

                let body = delivery.body.clone();
                match proto::GeoIpRequest::decode(&body[..]) {
                    Ok(rpc_message) => {
                        match rpc_message.message {
                            Some(proto::geo_ip_request::Message::IpLookup(ip_lookup_request)) => {
                                let addr = match ip_lookup_request.ip_addr {
                                    Some(proto::ip_lookup_request::IpAddr::Ipv4Addr(v4_addr_int)) => {
                                        std::net::IpAddr::V4(std::net::Ipv4Addr::from(v4_addr_int))
                                    },
                                    Some(proto::ip_lookup_request::IpAddr::Ipv6Addr(v6_addr_bytes)) => {
                                        std::net::IpAddr::V6(std::net::Ipv6Addr::from(TryInto::<[u8; 16]>::try_into(&v6_addr_bytes[0..16]).unwrap()))
                                    },
                                    _ => {
                                        rpc_consumer.nack(delivery, false).unwrap();
                                        continue;
                                    }
                                };
                                let resp = match mmdb_reader.lock().unwrap().lookup::<maxminddb::geoip2::City>(addr) {
                                    Ok(city_data) => {
                                        proto::IpLookupResponse {
                                            status: proto::ip_lookup_response::IpLookupStatus::Ok.into(),
                                            data: Some(proto::ip_lookup_response::IpLookupData {
                                                country: city_data.country.map_or(None,|c| c.iso_code.map(|s| s.to_string())),
                                                postal_code: city_data.postal.map_or(None,|c| c.code.map(|s| s.to_string())),
                                                time_zone: city_data.location.as_ref().map_or(None,|c| c.time_zone.map(|s| s.to_string())),
                                                metro_code: city_data.location.as_ref().map_or(None,|c| c.metro_code.map(|s| s.into())),
                                                latitude: city_data.location.as_ref().map_or(None,|c| c.latitude.map(|s| s.into())),
                                                longitude: city_data.location.map_or(None,|c| c.longitude.map(|s| s.into())),
                                                subdivisions: city_data.subdivisions.map_or(
                                                    vec![], |c| c.iter().filter_map(
                                                        |s| s.iso_code.map(|s| s.to_string())
                                                    ).collect()
                                                )
                                            })
                                        }
                                    },
                                    Err(maxminddb::MaxMindDBError::AddressNotFoundError(_)) => {
                                        proto::IpLookupResponse {
                                            status: proto::ip_lookup_response::IpLookupStatus::NotFound.into(),
                                            data: None
                                        }
                                    },
                                    _ => {
                                        proto::IpLookupResponse {
                                            status: proto::ip_lookup_response::IpLookupStatus::Unknown.into(),
                                            data: None
                                        }
                                    }
                                };
                                let mut resp_buf = vec![];
                                resp.encode(&mut resp_buf).unwrap();
                                amqp_channel.basic_publish("", amiquip::Publish::with_properties(
                                    &resp_buf, reply_to,
                                    amiquip::AmqpProperties::default().with_correlation_id(correlation_id),
                                )).unwrap();
                            },
                            None => {},
                        }
                        rpc_consumer.ack(delivery).unwrap();
                    }
                    Err(_) => {
                        rpc_consumer.ack(delivery).unwrap();
                    }
                }
            }
            other => {
                info!("RPC consumer ended: {:?}", other);
                break;
            }
        }
    }
}
