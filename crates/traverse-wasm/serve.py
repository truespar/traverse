#!/usr/bin/env python3
"""Tiny static server that emits the COEP/COOP headers required for
SharedArrayBuffer (and therefore the threaded WASM build).

Usage:
    cd crates/traverse-wasm
    python3 serve.py [port]    # default 8000

Then open http://localhost:8000/js/smoke.html

Cross-origin isolation is what unlocks SharedArrayBuffer, which is
what wasm-bindgen-rayon uses for thread pools. Without these headers,
the WASM module loads but fails at `initThreadPool` with a clear
error. See DESIGN.md section 4.3.
"""

import sys
from http.server import HTTPServer, SimpleHTTPRequestHandler


class CoiHandler(SimpleHTTPRequestHandler):
    def end_headers(self):
        # Required for SharedArrayBuffer to be available.
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        # The .wasm files need this CORP header to load cross-origin.
        # We're same-origin here but emit it anyway to match production.
        self.send_header("Cross-Origin-Resource-Policy", "same-origin")
        super().end_headers()

    # Quiet down the noisy access log; print the request line at INFO
    # level instead of stderr.
    def log_message(self, fmt, *args):
        sys.stdout.write(f"  {self.address_string()} - {fmt % args}\n")


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8000
    httpd = HTTPServer(("127.0.0.1", port), CoiHandler)
    print(f"Serving http://127.0.0.1:{port} with COEP/COOP headers")
    print(f"Smoke test: http://127.0.0.1:{port}/js/smoke.html")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print("\nstopped")
