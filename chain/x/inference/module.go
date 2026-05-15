// Package inference is the x/inference Cosmos SDK module.
//
// Real Cosmos SDK module wiring (AppModuleBasic, AppModule, ProvideModule
// for depinject) will be added when the chain is integrated with the SDK.
// For now this file documents the module's contract.

package inference

// ModuleName is the canonical name registered with the chain router.
const ModuleName = "inference"

// On-chain object types this module owns:
//   - Provider          (registered inference server)
//   - Job               (open bounty)
//   - Attestation       (signed proof of completion)
//
// Messages (Tx):
//   - MsgRegisterProvider
//   - MsgSubmitJob
//   - MsgClaimJob
//
// Events:
//   - EventProviderRegistered
//   - EventJobCreated
//   - EventJobClaimed
//
// Queries (gRPC):
//   - Providers(req QueryProvidersRequest) → QueryProvidersResponse
//   - Jobs(req QueryJobsRequest)         → QueryJobsResponse
//
// State storage layout:
//   /providers/<bech32>           → Provider
//   /jobs/<JobID>                 → Job
//   /attestations/<JobID>         → Attestation (after claim)
//
// See: the project notes §3 for the formal protobuf-style spec.
