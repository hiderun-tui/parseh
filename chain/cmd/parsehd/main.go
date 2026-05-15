// parsehd — PARSEH chain daemon.
//
// This is the entry point for the validator + RPC node.
// Today it is a stub that prints help and the build identity.
// The actual chain logic (Cosmos SDK app, ABCI handlers, x/inference module)
// is being built in the open at github.com/hiderun-tui/parseh.

package main

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
)

// Build-time information. Override via:
//   go build -ldflags "-X main.version=v0.0.1 -X main.commit=$(git rev-parse --short HEAD)"
var (
	version = "v0.0.0-dev"
	commit  = "unknown"
)

func main() {
	rootCmd := &cobra.Command{
		Use:   "parsehd",
		Short: "PARSEH chain daemon · validator and RPC node",
		Long: `parsehd is the node binary for the PARSEH humanitarian network chain.

This is a pre-genesis prototype. It does not produce blocks yet.
The actual Cosmos SDK + CometBFT integration lands in V0.1.

See: the project notes, the project notes,
	}

	rootCmd.AddCommand(versionCmd())
	rootCmd.AddCommand(initCmd())
	rootCmd.AddCommand(startCmd())

	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(1)
	}
}

func versionCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "version",
		Short: "Print build identity",
		Run: func(cmd *cobra.Command, args []string) {
			fmt.Printf("parsehd %s (%s)\n", version, commit)
			fmt.Println("PARSEH chain daemon · github.com/hiderun-tui/parseh")
			fmt.Println("License: Apache-2.0")
		},
	}
}

func initCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "init [moniker]",
		Short: "Initialise a node's home directory",
		Args:  cobra.ExactArgs(1),
		Run: func(cmd *cobra.Command, args []string) {
			fmt.Printf("STUB · would initialise node home dir for moniker=%q\n", args[0])
			fmt.Println("STUB · Cosmos SDK chain init will land in V0.1")
		},
	}
	return cmd
}

func startCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "start",
		Short: "Start the node",
		Run: func(cmd *cobra.Command, args []string) {
			fmt.Println("STUB · parsehd start")
			fmt.Println("STUB · CometBFT + ABCI loop will land in V0.1")
			fmt.Println("STUB · libp2p discovery will land in V0.1")
			fmt.Println()
			fmt.Println("This binary compiles to confirm the build chain is correct.")
			fmt.Println("Real chain code is being written. See V0 issues:")
			fmt.Println("  https://github.com/hiderun-tui/parseh/issues?q=is%3Aissue+label%3Av0+label%3Achain")
		},
	}
	return cmd
}
