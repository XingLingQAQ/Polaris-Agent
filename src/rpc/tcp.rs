use futures_util::{stream::FuturesUnordered, StreamExt};
use std::{collections::VecDeque, future::Future, io, net::SocketAddr, time::Duration};
use tokio::net::TcpStream;

const FALLBACK_DELAY: Duration = Duration::from_millis(250);

pub(super) async fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let addresses = tokio::net::lookup_host((host.trim_matches(['[', ']']), port)).await?;
    race_connections(interleave(addresses.collect()), |address| {
        TcpStream::connect(address)
    })
    .await
}

fn interleave(addresses: Vec<SocketAddr>) -> VecDeque<SocketAddr> {
    let Some(first) = addresses.first() else {
        return VecDeque::new();
    };
    let (mut preferred, mut alternate): (VecDeque<_>, VecDeque<_>) = addresses
        .iter()
        .copied()
        .partition(|address| address.is_ipv6() == first.is_ipv6());
    let mut ordered = VecDeque::new();
    while !preferred.is_empty() || !alternate.is_empty() {
        if let Some(address) = preferred.pop_front() {
            ordered.push_back(address);
        }
        if let Some(address) = alternate.pop_front() {
            ordered.push_back(address);
        }
    }
    ordered
}

async fn race_connections<T, F, Fut>(mut addresses: VecDeque<SocketAddr>, dial: F) -> io::Result<T>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    let mut attempts = FuturesUnordered::new();
    let mut last_error = io::Error::new(io::ErrorKind::NotFound, "host resolved to no addresses");
    let mut next_attempt = tokio::time::Instant::now();
    loop {
        if attempts.is_empty() {
            let Some(address) = addresses.pop_front() else {
                return Err(last_error);
            };
            attempts.push(dial(address));
            next_attempt = tokio::time::Instant::now() + FALLBACK_DELAY;
        }
        tokio::select! {
            Some(result) = attempts.next() => match result {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = error,
            },
            _ = tokio::time::sleep_until(next_attempt), if !addresses.is_empty() => {
                attempts.push(dial(addresses.pop_front().unwrap()));
                next_attempt = tokio::time::Instant::now() + FALLBACK_DELAY;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addresses() -> VecDeque<SocketAddr> {
        interleave(vec![
            "[::1]:443".parse().unwrap(),
            "[::2]:443".parse().unwrap(),
            "127.0.0.1:443".parse().unwrap(),
        ])
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_ipv6_does_not_block_ipv4() {
        let start = tokio::time::Instant::now();
        let address = race_connections(addresses(), |address| async move {
            if address.is_ipv6() {
                std::future::pending::<()>().await;
            }
            Ok(address)
        })
        .await
        .unwrap();
        assert!(address.is_ipv4());
        assert_eq!(start.elapsed(), FALLBACK_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn working_ipv6_remains_preferred() {
        let start = tokio::time::Instant::now();
        let address = race_connections(addresses(), |address| async move { Ok(address) })
            .await
            .unwrap();
        assert!(address.is_ipv6());
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn immediate_failures_advance_without_a_delay() {
        let start = tokio::time::Instant::now();
        let address = race_connections(addresses(), |address| async move {
            if address.is_ipv6() {
                return Err(io::Error::from(io::ErrorKind::ConnectionRefused));
            }
            Ok(address)
        })
        .await
        .unwrap();
        assert!(address.is_ipv4());
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test]
    async fn empty_and_failed_addresses_return_errors() {
        assert!(
            race_connections(VecDeque::new(), |_: SocketAddr| async { Ok(()) })
                .await
                .is_err()
        );
        let error = race_connections(addresses(), |_| async {
            Err::<(), _>(io::Error::from(io::ErrorKind::ConnectionRefused))
        })
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[tokio::test]
    async fn connects_to_a_real_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = connect("localhost", listener.local_addr().unwrap().port())
            .await
            .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), listener.local_addr().unwrap());
    }
}
