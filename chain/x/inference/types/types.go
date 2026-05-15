// Package types holds the protobuf-generated and hand-written type
// definitions for the x/inference module — the on-chain bookkeeping
// for distributed LLM inference contributions.
//
// Current state: hand-written stubs. Real .proto-generated types will
// land alongside the V0.1 Cosmos SDK integration.

package types

import (
	"errors"
	"time"
)

// ModuleName is the keeper's module identifier on the chain.
const ModuleName = "inference"

// StoreKey is the global persistent-store key for this module.
const StoreKey = ModuleName

// JobID uniquely identifies an inference job submitted to the network.
type JobID string

// Validate is a placeholder for richer ID checks; today it only requires
// non-empty.
func (id JobID) Validate() error {
	if id == "" {
		return errors.New("inference: empty JobID")
	}
	return nil
}

// Provider is a node that has registered to serve inference requests.
type Provider struct {
	Address       string    // bech32 PARSEH address
	GPUMemoryMB   uint32    // self-reported VRAM
	ModelTags     []string  // e.g. "llama-3.1:8b", "qwen2.5:7b"
	JoinedAt      time.Time // first registration
	ReputationBps uint16    // 0–10000 (basis points)
}

// Job represents a single inference request awaiting fulfilment.
type Job struct {
	ID         JobID
	Requester  string    // bech32 PARSEH address
	ModelTag   string    // requested model identifier
	PromptHash [32]byte  // sha256 of the user prompt (privacy)
	MaxTokens  uint32
	Bounty     uint64 // microPARSEH offered
	CreatedAt  time.Time
}

// Attestation is the signed proof a provider returns when it has
// completed a job. The actual signature scheme + verification logic
// lives in the keeper.
type Attestation struct {
	JobID       JobID
	Provider    string
	OutputHash  [32]byte
	TokensUsed  uint32
	WallSeconds uint32
	Signature   []byte // ed25519 of the provider's chain key
}
