PARSEH — V0.1-alpha portable Windows build
============================================

What this is
------------

PARSEH is an open-source humanitarian network. Your computer becomes
one of the volunteer nodes that gives other people in censored regions
free encrypted access to the internet and AI.

There is no company behind this. No subscription. No telemetry.
This is community-built free software under Apache 2.0.

Repository: https://github.com/hiderun-tui/parseh
Website:    https://hiderun.com
License:    Apache 2.0 (see LICENSE.txt)

What's in this ZIP
------------------

parseh-miner.exe              The volunteer node. Run this.
README.txt                    What you're reading.
LICENSE.txt                   Apache 2.0 license text.
install-as-startup.bat        Optional: run miner at Windows login.
uninstall.bat                 Removes the autostart entry.
examples/miner.toml           Default config (auto-generated on first run).

How to run
----------

  1. Extract this ZIP anywhere (e.g. C:\Programs\parseh)
  2. Double-click parseh-miner.exe
  3. On first run, it will:
       a. Generate your wallet (an ed25519 keypair, like a Bitcoin wallet)
       b. Save it to %APPDATA%\PARSEH
       c. Check if you have Ollama or a GGUF LLM model locally
       d. If you don't, ask permission to download TinyLlama (~640 MB)
       e. Join the PARSEH network and start advertising your capabilities

  4. After setup, the miner runs quietly. Other nodes can discover you
     and route work your way. You earn PARSEH (the network's currency)
     for the bandwidth and compute you contribute.

Privacy + safety
----------------

  - No telemetry. No analytics. No data leaves your machine without
    your explicit consent (the only external HTTP request is the LLM
    model download, and that requires your YES first).
  - The wallet private key is stored encrypted at rest.
  - Network traffic uses Noise encryption (libp2p stack).
  - You can run a relay-only node (no LLM) by setting:
      [capabilities]
      inference = false
      relay     = true
    in your miner.toml.

Stopping the miner
------------------

Close the console window. There is no background service. Run
uninstall.bat if you previously enabled autostart.

Reporting issues
----------------

GitHub Issues: https://github.com/hiderun-tui/parseh/issues

This is V0.1-alpha. Many features are incomplete. The "Read also"
section of the GitHub README points to the roadmap, contribution
workflow, and security model.
