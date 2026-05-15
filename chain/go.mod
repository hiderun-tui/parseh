module github.com/hiderun-tui/parseh/chain

go 1.23.0

require github.com/spf13/cobra v1.8.0

require (
	github.com/inconshreveable/mousetrap v1.1.0 // indirect
	github.com/spf13/pflag v1.0.10 // indirect
)

// Real Cosmos SDK dependencies will be added when the chain is fleshed out.
// Pinned versions live here so contributors get a deterministic build.
//
// Planned (uncomment when adding the actual chain logic):
//
// require (
//     cosmossdk.io/api v0.7.5
//     cosmossdk.io/core v0.11.0
//     cosmossdk.io/depinject v1.0.0
//     cosmossdk.io/log v1.3.1
//     cosmossdk.io/math v1.3.0
//     cosmossdk.io/store v1.0.2
//     cosmossdk.io/x/tx v0.13.1
//     github.com/cometbft/cometbft v0.38.5
//     github.com/cosmos/cosmos-sdk v0.50.4
//     github.com/cosmos/gogoproto v1.4.10
//     google.golang.org/grpc v1.61.0
// )
