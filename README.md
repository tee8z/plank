# Plank

A lightweight, secure Bitcoin wallet designed for the Mutinynet signet network, now as a terminal user interface (TUI) application. Built in Rust, Plank provides a fast, efficient, and intuitive experience for developers and testers working with Bitcoin on Mutinynet. The wallet operates entirely offline when needed, ensuring privacy and security.

## Features

- 🔐 **Secure Wallet Management**
  - Hierarchical Deterministic (HD) wallet support (BIP32/39/44/84)
  - Mnemonic phrase backup and recovery
  - Secure key creation using Rust cryptography

- 💰 **Transaction Management**
  - Send and receive Bitcoin on Mutinynet
  - View transaction history
  - Real-time balance updates

- 🖥️ **Terminal User Interface**
  - Fast, keyboard-driven UI with `ratatui`
  - Works entirely in the terminal
  - Responsive and accessible design

- 🛠️ **Developer Friendly**
  - Modular, well-documented Rust codebase
  - Comprehensive error handling
  - Detailed transaction inspection

- 📴 **Offline Capability**
  - All sensitive operations are performed locally
  - Wallet can be used offline (view balances, sign transactions, manage keys)
  - Network access only required for broadcasting and syncing

## Technical Stack

- **Language:** Rust (2021 edition)
- **UI:** [`ratatui`](https://github.com/ratatui-org/ratatui) for terminal UI
- **Wallet:** [`bdk_wallet`](https://github.com/bitcoindevkit/bdk) for Bitcoin wallet functionality
- **Network:** [`bdk_esplora`](https://github.com/bitcoindevkit/bdk) for Esplora API integration
- **Configuration:**
  - [`dirs`](https://docs.rs/dirs/latest/dirs/) for standard config file locations
  - [`clap`](https://github.com/clap-rs/clap) for command-line argument parsing
- **Build & Tooling:**
  - Cargo for building and dependency management
  - Rustfmt and Clippy for code quality

## Getting Started

### Prerequisites

- Rust (latest stable, see [rustup.rs](https://rustup.rs))
- Cargo (comes with Rust)
- Internet connection (for initial build and online features)

### Installation

1. Clone the repository:
   ```bash
   git clone https://github.com/w3ird-tech/plank.git
   cd plank
   ```

2. Build the application:
   ```bash
   cargo build --release
   ```

3. Run the application:
   ```bash
   cargo run --release
   ```

### Configuration

The application supports configuration via:

1. **Config File:**
   - Place a `config.toml` file in the standard configuration directory for your OS (e.g., `$XDG_CONFIG_HOME/plank/config.toml` on Linux, `%APPDATA%\plank\config.toml` on Windows, or `~/Library/Application Support/plank/config.toml` on macOS).
   - The location is determined automatically using the `dirs` crate.

   Example `config.toml`:
   ```toml
   esplora_url = "https://mutinynet.com/api"
   offline = false
   ```
   - `esplora_url`: The URL of the Mutinynet Esplora instance to use for blockchain data.
   - `offline`: If set to `true`, the wallet will not attempt any network connections (view balances, sign transactions, manage keys only).

   > **Note:** Wallet data (keys, cache, etc.) is stored in the standard data directory for your OS (e.g., `$XDG_DATA_HOME/plank/`), which is separate from the config directory. Both the config and data locations are determined automatically using the `dirs` crate and do not need to be configured manually.
2. **Command-Line Flags:**
   - You can override any config file value by providing flags when running the app. Flags are parsed using the `clap` crate.
   - Example:
     ```bash
     cargo run --release -- --esplora-url https://mutinynet.com/api --offline
     ```

**Precedence:** Command-line flags > config file > built-in defaults.

(You may still use environment variables if you wish, but config file and CLI flags are preferred.)

## Security Considerations

- The wallet is designed for use with Mutinynet (a Bitcoin signet) and should NOT be used with real bitcoins.
- Private keys are stored securely on your local machine (encrypted file or OS keyring; see documentation for details).
- Always back up your mnemonic phrase in a secure location.
- All sensitive operations are performed client-side; no data is sent to third-party servers except for Esplora API calls.
- This is experimental software. Use at your own risk.

## Development

### Key Components

1. **Wallet Manager (bdk_wallet)**
   - Handles HD wallet generation, storage, and recovery
   - Manages wallet state and transaction signing

2. **Network Service (bdk_esplora)**
   - Connects to Mutinynet Esplora instance for blockchain data
   - Handles transaction broadcasting and syncing
   - Can be disabled for offline mode

3. **TUI Components (ratatui)**
   - Send/Receive screens
   - Transaction history
   - Network status indicator
   - Mnemonic backup/recovery
   - Settings and help screens

## Security Considerations

- Private keys are stored securely on your local machine (encrypted file or OS keyring; see documentation for details).
- All sensitive operations are performed locally on your device.
- No server-side component is required.
- Users should be aware of the security implications of storing wallet data on their local machine.

## Development

### Common Commands

All development tasks are managed via [just](https://github.com/casey/just) commands. To see all available commands, simply run:

```bash
just
```

Common commands include:
- `just run` — Run the application
- `just build` — Build the application
- `just fmt` — Format code
- `just lint` — Lint code (runs clippy)

> Running `just` with no arguments will list all available commands.


## Contributing

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/AmazingFeature`)
3. Commit your changes (`git commit -m 'Add some AmazingFeature'`)
4. Push to the branch (`git push origin feature/AmazingFeature`)
5. Open a Pull Request

Please ensure your code is formatted with `cargo fmt` and passes `cargo clippy` and tests before submitting.

## License

This project is licensed under the GNU Affero General Public License v3.0 - see the [LICENSE](LICENSE) file for details.

## Acknowledgments

- [BDK (Bitcoin Dev Kit)](https://bitcoindevkit.org/) for wallet and blockchain functionality
- [ratatui](https://github.com/ratatui-org/ratatui) for terminal UI components
- [clap](https://github.com/clap-rs/clap) for command-line argument parsing
- [dirs](https://docs.rs/dirs/latest/dirs/) for cross-platform config/data directory management
- The Mutinynet community for their signet infrastructure
- All the open-source projects and contributors that made this wallet possible
