#!/usr/bin/env python3
"""Simple file server for showcase demos."""
import http.server, os
PORT = 8090
DIR = os.path.dirname(os.path.abspath(__file__))
os.chdir(DIR)
print(f"Showcase on :{PORT} — open http://localhost:{PORT}/")
http.server.HTTPServer(("", PORT), http.server.SimpleHTTPRequestHandler).serve_forever()
