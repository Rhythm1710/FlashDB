#!/bin/sh
# Write playground/config.js from FLASHDB_WS_URL so a static Vercel build
# points the page at the Railway WebSocket. `?ws=` still overrides it.
set -eu
node -e 'const fs = require("fs"); const url = process.env.FLASHDB_WS_URL || ""; fs.writeFileSync("playground/config.js", "window.FLASHDB_WS_URL = " + JSON.stringify(url) + ";\n");'
