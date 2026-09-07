# Security

Report vulnerabilities to hello@flightportrait.com. A human reads
that address. Do not open a public issue for a report that could
harm feeders.

stationd serves its setup wizard and status page on the LAN without
authentication, by design, like a router's first-run page; do not
expose its listen address to the internet. It holds the station keys
in station.toml with the file permissions of the user running it, and
passes them to rx and mlatc on their command lines. Nothing it serves
contains a key.
