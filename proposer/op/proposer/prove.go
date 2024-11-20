package proposer

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"strings"
	"time"

	"github.com/ethereum/go-ethereum/accounts/abi/bind"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent/proofrequest"
)

const PROOF_STATUS_TIMEOUT = 30 * time.Second
const WITNESS_GEN_TIMEOUT = 20 * time.Minute

// This limit is set to prevent overloading the witness generation server. Until Kona improves their native I/O API (https://github.com/anton-rs/kona/issues/553)
// the maximum number of concurrent witness generation requests is roughly num_cpu / 2. Set it to 5 for now to be safe.
const MAX_CONCURRENT_WITNESS_GEN = 5

// Process all of the pending proofs.
func (l *L2OutputSubmitter) ProcessPendingProofs() error {
	// Get all proof requests that are currently in the PROVING state.
	reqs, err := l.db.GetAllProofsWithStatus(proofrequest.StatusPROVING)
	if err != nil {
		return err
	}
	for _, req := range reqs {
		proofStatus, err := l.GetProofStatus(req.ProverRequestID)
		if err != nil {
			l.Log.Error("failed to get proof status for ID", "id", req.ProverRequestID, "err", err)

			// Record the error for the get proof status call.
			l.Metr.RecordError("get_proof_status", 1)
			return err
		}
		if proofStatus.Status == SP1ProofStatusFulfilled {
			// Update the proof in the DB and update status to COMPLETE.
			l.Log.Info("Fulfilled Proof", "id", req.ProverRequestID)
			err = l.db.AddFulfilledProof(req.ID, proofStatus.Proof)
			if err != nil {
				l.Log.Error("failed to update completed proof status", "err", err)
				return err
			}
			continue
		}

		// TODO: Is this proof timeout logic necessary? Users should be able to count on the proof being fulfilled or unclaimed.
		timeout := uint64(time.Now().Unix()) > req.ProofRequestTime+l.DriverSetup.Cfg.ProofTimeout
		if timeout || proofStatus.Status == SP1ProofStatusUnclaimed {
			// Record the failure reason.
			if timeout {
				l.Log.Info("Proof timed out", "id", req.ProverRequestID)
				l.Metr.RecordProveFailure("Timeout")
			} else {
				l.Log.Info("Proof unclaimed", "id", req.ProverRequestID, "reason", proofStatus.UnclaimDescription.String())
				l.Metr.RecordProveFailure(proofStatus.UnclaimDescription.String())
			}

			err = l.RetryRequest(req, proofStatus)
			if err != nil {
				return fmt.Errorf("failed to retry request: %w", err)
			}
		}
	}

	return nil
}

// Retry a proof request. Sets the status of a proof to FAILED and retries the proof based on the optional proof status response.
// If the response is a program execution error, the proof is split into two, which will avoid SP1 out of memory execution errors.
func (l *L2OutputSubmitter) RetryRequest(req *ent.ProofRequest, status ProofStatusResponse) error {
	err := l.db.UpdateProofStatus(req.ID, proofrequest.StatusFAILED)
	if err != nil {
		l.Log.Error("failed to update proof status", "err", err)
		return err
	}

	// If the proof was unclaimed due to a program execution error, we should split the proof into two.
	if status.UnclaimDescription == ProgramExecutionError {
		mid := (req.StartBlock + req.EndBlock) / 2
		// Create two new proof requests, one from [start, mid] and one from [mid, end]. The requests
		// are consecutive and overlapping.
		err = l.db.NewEntry(req.Type, req.StartBlock, mid)
		if err != nil {
			l.Log.Error("failed to add first proof request", "err", err)
			return err
		}
		err = l.db.NewEntry(req.Type, mid, req.EndBlock)
		if err != nil {
			l.Log.Error("failed to add second proof request", "err", err)
			return err
		}
	} else {
		// If the proof was unclaimed for any other reason, retry with the same range.
		err = l.db.NewEntry(req.Type, req.StartBlock, req.EndBlock)
		if err != nil {
			l.Log.Error("failed to add proof request", "err", err)
			return err
		}
	}

	return nil
}

func (l *L2OutputSubmitter) RequestQueuedProofs(ctx context.Context) error {
	nextProofToRequest, err := l.db.GetNextUnrequestedProof()
	if err != nil {
		return fmt.Errorf("failed to get unrequested proofs: %w", err)
	}
	if nextProofToRequest == nil {
		return nil
	}

	if nextProofToRequest.Type == proofrequest.TypeAGG {
		if nextProofToRequest.L1BlockHash == "" {
			blockNumber, blockHash, err := l.checkpointBlockHash(ctx)
			if err != nil {
				l.Log.Error("failed to checkpoint block hash", "err", err)
				return err
			}
			nextProofToRequest, err = l.db.AddL1BlockInfoToAggRequest(nextProofToRequest.StartBlock, nextProofToRequest.EndBlock, blockNumber, blockHash.Hex())
			if err != nil {
				l.Log.Error("failed to add L1 block info to AGG request", "err", err)
			}

			// wait for the next loop so that we have the version with the block info added
			return nil
		} else {
			l.Log.Info("found agg proof with already checkpointed l1 block info")
		}
	} else {
		witnessGenProofs, err := l.db.GetNumberOfRequestsWithStatuses(proofrequest.StatusWITNESSGEN)
		if err != nil {
			return fmt.Errorf("failed to count witnessgen proofs: %w", err)
		}
		provingProofs, err := l.db.GetNumberOfRequestsWithStatuses(proofrequest.StatusPROVING)
		if err != nil {
			return fmt.Errorf("failed to count proving proofs: %w", err)
		}

		// The number of witness generation requests is capped at MAX_CONCURRENT_WITNESS_GEN. This prevents overloading the machine with processes spawned by the witness generation server.
		// Once https://github.com/anton-rs/kona/issues/553 is fixed, we may be able to remove this check.
		if witnessGenProofs >= MAX_CONCURRENT_WITNESS_GEN {
			l.Log.Info("max witness generation reached, waiting for next cycle")
			return nil
		}

		// The total number of concurrent proofs is capped at MAX_CONCURRENT_PROOF_REQUESTS.
		if (witnessGenProofs + provingProofs) >= int(l.Cfg.MaxConcurrentProofRequests) {
			l.Log.Info("max concurrent proof requests reached, waiting for next cycle")
			return nil
		}
	}
	go func(p ent.ProofRequest) {
		l.Log.Info("requesting proof from server", "type", p.Type, "start", p.StartBlock, "end", p.EndBlock, "id", p.ID)
		// Set the proof status to WITNESSGEN.
		err = l.db.UpdateProofStatus(p.ID, proofrequest.StatusWITNESSGEN)
		if err != nil {
			l.Log.Error("failed to update proof status", "err", err)
			return
		}

		// Request the type of proof depending on the mock configuration.
		err = l.RequestProof(p, l.Cfg.Mock)
		if err != nil {
			// If the proof fails to be requested, we should add it to the queue to be retried.
			err = l.RetryRequest(nextProofToRequest, ProofStatusResponse{})
			if err != nil {
				l.Log.Error("failed to retry request", "err", err)
			}

		}
	}(*nextProofToRequest)

	return nil
}

// Use the L2OO contract to look up the range of blocks that the next proof must cover.
// Check the DB to see if we have sufficient span proofs to request an agg proof that covers this range.
// If so, queue up the agg proof in the DB to be requested later.
func (l *L2OutputSubmitter) DeriveAggProofs(ctx context.Context) error {
	latest, err := l.l2ooContract.LatestBlockNumber(&bind.CallOpts{Context: ctx})
	if err != nil {
		return fmt.Errorf("failed to get latest L2OO output: %w", err)
	}

	// This fetches the next block number, which is the currentBlock + submissionInterval.
	minTo, err := l.l2ooContract.NextBlockNumber(&bind.CallOpts{Context: ctx})
	if err != nil {
		return fmt.Errorf("failed to get next L2OO output: %w", err)
	}

	l.Log.Info("Checking for AGG proof", "blocksToProve", minTo.Uint64()-latest.Uint64(), "latestProvenBlock", latest.Uint64(), "minBlockToProveToAgg", minTo.Uint64())
	created, end, err := l.db.TryCreateAggProofFromSpanProofs(latest.Uint64(), minTo.Uint64())
	if err != nil {
		return fmt.Errorf("failed to create agg proof from span proofs: %w", err)
	}
	if created {
		l.Log.Info("created new AGG proof", "from", latest.Uint64(), "to", end)
	}

	return nil
}

// ProofRequestConfig holds the configuration for a proof request
type ProofRequestConfig struct {
	isMock    bool
	proofType proofrequest.Type
	start     uint64
	end       uint64
	// Optional, only used for agg proofs.
	l1Hash    string
}

// RequestProof handles both mock and real proof requests
func (l *L2OutputSubmitter) RequestProof(p ent.ProofRequest, isMock bool) error {
	config := ProofRequestConfig{
		isMock:    isMock,
		proofType: p.Type,
		start:     p.StartBlock,
		end:       p.EndBlock,
		l1Hash:    p.L1BlockHash,
	}

	var result interface{}
	var err error

	if p.Type == proofrequest.TypeAGG {
		result, err = l.requestAggProof(config)
	} else if p.Type == proofrequest.TypeSPAN {
		result, err = l.requestSpanProof(config)
	} else {
		return fmt.Errorf("unknown proof type: %s", p.Type)
	}

	if err != nil {
		l.Log.Error("witnessgen request failed", "err", err, "proof", p)
		l.Metr.RecordError("witnessgen", 1)
		return fmt.Errorf("failed to request %s proof: %w", p.Type, err)
	}

	// Update the proof status to PROVING because AddFulfilledProof checks that the proof status is PROVING before adding the proof to the DB.
	if err := l.db.UpdateProofStatus(p.ID, proofrequest.StatusPROVING); err != nil {
		return fmt.Errorf("failed to set proof status to proving: %w", err)
	}

	if isMock {
		// Add the mock proofs directly as fulfilled proofs to the DB. The proof field from `requestSpanProof` and `requestAggProof` is already bytes in mock mode.
		return l.db.AddFulfilledProof(p.ID, result.([]byte))
	}

	// For real proofs, the result is a string.
	proofId := result.(string)
	return l.db.SetProverRequestID(p.ID, proofId)
}

func (l *L2OutputSubmitter) requestSpanProof(config ProofRequestConfig) (interface{}, error) {
	if config.start >= config.end {
		return nil, fmt.Errorf("start must be less than end")
	}

	l.Log.Info("requesting span proof", "start", config.start, "end", config.end)
	requestBody := SpanProofRequest{
		Start: config.start,
		End:   config.end,
	}
	jsonBody, err := json.Marshal(requestBody)
	if err != nil {
		return nil, fmt.Errorf("failed to marshal request body: %w", err)
	}

	return l.requestProofFromServer(config.proofType, jsonBody, config.isMock)
}

// requestAggProof returns the proof ID from the server in real mode, and an empty string in mock mode.
func (l *L2OutputSubmitter) requestAggProof(config ProofRequestConfig) (interface{}, error) {
	l.Log.Info("requesting agg proof", "start", config.start, "end", config.end)

	subproofs, err := l.db.GetConsecutiveSpanProofs(config.start, config.end)
	if err != nil {
		return nil, fmt.Errorf("failed to get subproofs: %w", err)
	}
	requestBody := AggProofRequest{
		Subproofs: subproofs,
		L1Head:    config.l1Hash,
	}
	jsonBody, err := json.Marshal(requestBody)
	if err != nil {
		return nil, fmt.Errorf("failed to marshal request body: %w", err)
	}

	return l.requestProofFromServer(config.proofType, jsonBody, config.isMock)
}

// requestProofFromServer returns the proof ID from the server in real mode, and the proof bytes in mock mode.
func (l *L2OutputSubmitter) requestProofFromServer(proofType proofrequest.Type, jsonBody []byte, isMock bool) (interface{}, error) {
	urlPath := l.getProofEndpoint(proofType, isMock)

	req, err := http.NewRequest("POST", l.Cfg.OPSuccinctServerUrl+"/"+urlPath, bytes.NewBuffer(jsonBody))
	if err != nil {
		return nil, fmt.Errorf("failed to create request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")

	client := &http.Client{Timeout: WITNESS_GEN_TIMEOUT}
	resp, err := client.Do(req)
	if err != nil {
		if netErr, ok := err.(net.Error); ok && netErr.Timeout() {
			l.Metr.RecordWitnessGenFailure("Timeout")
			return nil, fmt.Errorf("request timed out after %s: %w", WITNESS_GEN_TIMEOUT, err)
		}
		return nil, fmt.Errorf("failed to send request: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		l.Metr.RecordWitnessGenFailure("Failed")
		return nil, fmt.Errorf("received non-200 status code: %d", resp.StatusCode)
	}

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, fmt.Errorf("error reading response body: %w", err)
	}

	if isMock {
		var response ProofStatusResponse
		if err := json.Unmarshal(body, &response); err != nil {
			return nil, fmt.Errorf("error decoding JSON response: %w", err)
		}
		l.Log.Info("successfully produced mock proof")
		fmt.Printf("Length of mock proof [requestProofFromServer]: %d\n", len(response.Proof))

		// TODO: Due to a bug in sp1-sdk, the length of the proof returned from `.bytes()` is 4 for mock groth16 proofs. Until
		// https://github.com/succinctlabs/sp1/pull/1802 is merged and included in a new release, we need to manually return
		// an empty byte slice for agg proofs. Once it's merged we can just return response.Proof.
		if proofType == proofrequest.TypeAGG {
			return []byte{}, nil
		} else {
			return response.Proof, nil
		}
	} else {
		var response WitnessGenerationResponse
		if err := json.Unmarshal(body, &response); err != nil {
			return nil, fmt.Errorf("error decoding JSON response: %w", err)
		}
		l.Log.Info("successfully submitted proof", "proofID", response.ProofID)
		return response.ProofID, nil
	}
}

func (l *L2OutputSubmitter) getProofEndpoint(proofType proofrequest.Type, isMock bool) string {
	if isMock {
		if proofType == proofrequest.TypeAGG {
			return "request_mock_agg_proof"
		}
		return "request_mock_span_proof"
	}
	if proofType == proofrequest.TypeAGG {
		return "request_agg_proof"
	}
	return "request_span_proof"
}

// Get the status of a proof given its ID.
func (l *L2OutputSubmitter) GetProofStatus(proofId string) (ProofStatusResponse, error) {
	req, err := http.NewRequest("GET", l.Cfg.OPSuccinctServerUrl+"/status/"+proofId, nil)
	if err != nil {
		return ProofStatusResponse{}, fmt.Errorf("failed to create request: %w", err)
	}

	client := &http.Client{
		Timeout: PROOF_STATUS_TIMEOUT,
	}
	resp, err := client.Do(req)
	if err != nil {
		if err, ok := err.(net.Error); ok && err.Timeout() {
			return ProofStatusResponse{}, fmt.Errorf("request timed out after %s: %w", PROOF_STATUS_TIMEOUT, err)
		}
		return ProofStatusResponse{}, fmt.Errorf("failed to send request: %w", err)
	}
	defer resp.Body.Close()

	// If the response status code is not 200, return an error.
	if resp.StatusCode != http.StatusOK {
		return ProofStatusResponse{}, fmt.Errorf("received non-200 status code: %d", resp.StatusCode)
	}

	// Read the response body
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return ProofStatusResponse{}, fmt.Errorf("error reading the response body: %v", err)
	}

	// Create a variable of the Response type
	var proofStatus ProofStatusResponse

	// Unmarshal the JSON into the response variable
	err = json.Unmarshal(body, &proofStatus)
	if err != nil {
		return ProofStatusResponse{}, fmt.Errorf("error decoding JSON response: %v", err)
	}

	return proofStatus, nil
}

// Validate the contract's configuration of the aggregation and range verification keys as well
// as the rollup config hash.
func (l *L2OutputSubmitter) ValidateConfig(address string) error {
	l.Log.Info("requesting config validation", "address", address)
	requestBody := ValidateConfigRequest{
		Address: address,
	}
	jsonBody, err := json.Marshal(requestBody)
	if err != nil {
		return fmt.Errorf("failed to marshal request body: %w", err)
	}

	req, err := http.NewRequest("POST", l.Cfg.OPSuccinctServerUrl+"/validate_config", bytes.NewBuffer(jsonBody))
	if err != nil {
		return fmt.Errorf("failed to create request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")

	client := &http.Client{
		Timeout: PROOF_STATUS_TIMEOUT,
	}

	// Attempt to validate the config up to 5 times with exponential backoff.
	maxRetries := 5
	backoff := 1 * time.Second
	var resp *http.Response

	for i := 0; i < maxRetries; i++ {
		resp, err = client.Do(req)
		if err == nil && resp.StatusCode == http.StatusOK {
			break
		}
		if i == maxRetries-1 {
			if err != nil {
				if err, ok := err.(net.Error); ok && err.Timeout() {
					return fmt.Errorf("request timed out after %s: %w", PROOF_STATUS_TIMEOUT, err)
				}
				return fmt.Errorf("failed to send request: %w", err)
			}
			return fmt.Errorf("server not healthy after %d retries", maxRetries)
		}

		l.Log.Info("server not ready, retrying", "attempt", i+1, "backoff", backoff)
		time.Sleep(backoff)
		backoff *= 2
	}
	defer resp.Body.Close()

	// Read the response body
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return fmt.Errorf("error reading the response body: %v", err)
	}

	// Create a variable of the ValidateConfigResponse type
	var response ValidateConfigResponse

	// Unmarshal the JSON into the response variable
	err = json.Unmarshal(body, &response)
	if err != nil {
		return fmt.Errorf("error decoding JSON response: %v", err)
	}

	var invalidConfigs []string
	if !response.RollupConfigHashValid {
		invalidConfigs = append(invalidConfigs, "rollup config hash")
	}
	if !response.AggVkeyValid {
		invalidConfigs = append(invalidConfigs, "aggregation verification key")
	}
	if !response.RangeVkeyValid {
		invalidConfigs = append(invalidConfigs, "range verification key")
	}
	if len(invalidConfigs) > 0 {
		return fmt.Errorf("config is invalid: %s", strings.Join(invalidConfigs, ", "))
	}

	return nil
}
