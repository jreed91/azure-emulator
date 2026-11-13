// Resolves every *.localhost name to 127.0.0.1, as browsers and systemd do but
// Node's resolver here does not, so per-namespace hostnames reach the emulator.
const dns = require("dns");
const lookup = dns.lookup;
dns.lookup = (host, options, callback) =>
  lookup(typeof host === "string" && host.endsWith(".localhost") ? "127.0.0.1" : host, options, callback);
