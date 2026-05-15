// Package keeper holds the inference module's state-modifying logic.
//
// In a real Cosmos SDK module this file imports sdk.Context, store.KVStore,
// and the proto-generated message types. For now we have a hand-written
// stub so contributors can see where to start.

package keeper

import (
	"github.com/hiderun-tui/parseh/chain/x/inference/types"
)

// Keeper is the state authority for the inference module.
// In V0.1 this gains:
//   - storeKey       store.Key
//   - cdc            codec.BinaryCodec
//   - bankKeeper     types.BankKeeper (for bounty escrow)
//   - paramstore     paramtypes.Subspace
type Keeper struct {
	// placeholder fields — replaced when ABCI integration lands
	providers map[string]types.Provider
	jobs      map[types.JobID]types.Job
}

// NewKeeper constructs a zero-value keeper. Real signature will accept
// store keys and the bank module reference.
func NewKeeper() Keeper {
	return Keeper{
		providers: map[string]types.Provider{},
		jobs:      map[types.JobID]types.Job{},
	}
}

// RegisterProvider records a new inference provider.
// V0.1 will gate this on a stake deposit and emit a typed event.
func (k *Keeper) RegisterProvider(p types.Provider) error {
	if p.Address == "" {
		return ErrInvalidProvider
	}
	k.providers[p.Address] = p
	return nil
}

// SubmitJob places a job onto the bounty queue.
func (k *Keeper) SubmitJob(j types.Job) error {
	if err := j.ID.Validate(); err != nil {
		return err
	}
	k.jobs[j.ID] = j
	return nil
}

// ClaimJob marks a job as fulfilled and releases the bounty.
// V0.1 will verify the Attestation signature, slash on bad output,
// and pay the bounty via the bank keeper.
func (k *Keeper) ClaimJob(a types.Attestation) error {
	if _, ok := k.jobs[a.JobID]; !ok {
		return ErrJobNotFound
	}
	delete(k.jobs, a.JobID)
	return nil
}
