package proposer

import (
	"context"
	"fmt"
	"time"

	"github.com/ethereum-optimism/optimism/op-service/dial"
	"github.com/ethereum-optimism/optimism/op-service/sources"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/log"
	gethrpc "github.com/ethereum/go-ethereum/rpc"
	"github.com/succinctlabs/op-succinct-go/proposer/db"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent"
	"github.com/succinctlabs/op-succinct-go/proposer/db/ent/proofrequest"
)

type L2OutputSubmitterer interface {
	GetL1HeadForL2Block(ctx context.Context, rollupClient *sources.RollupClient, l2End uint64) (uint64, error)
}

// Define a new Proofs API that will provider the Proofs API to the RPC server
type ProofsAPI struct {
	logger    log.Logger
	db        db.ProofDB
	mock      bool
	driver    L2OutputSubmitterer
	rollupRPC string
}

func NewProofsAPI(db db.ProofDB, logger log.Logger, mock bool, driver *L2OutputSubmitter) (*ProofsAPI, error) {
	return &ProofsAPI{
		logger:    logger,
		db:        db,
		mock:      mock,
		driver:    driver,
		rollupRPC: driver.Cfg.RollupRpc,
	}, nil
}

// GetProofsAPI returns the ProofsAPI struct
func GetProofsAPI(api *ProofsAPI) gethrpc.API {
	return gethrpc.API{
		Namespace: "proofs",
		Service:   api,
	}
}

type RequestAggProofResponse struct {
	StartBlock     uint64 `json:"start_block"`
	EndBlock       uint64 `json:"end_block"`
	ProofRequestID string `json:"proof_request_id"`
}

func (pa *ProofsAPI) RequestAggProof(ctx context.Context, startBlock, maxBlock, l1BlockNumber uint64, l1BlockHash common.Hash) (*RequestAggProofResponse, error) {
	pa.logger.Info("requesting agg proof from server", "start", startBlock, "max end", maxBlock)

	// Return the proof request ID or the mock proof request ID
	proverRequestID := "mock_prover_request_id"

	maxBlock, err := pa.maxBlockL1Limit(ctx, maxBlock, l1BlockNumber)
	if err != nil {
		return nil, fmt.Errorf("failed to get max block L1 limit: %w", err)
	}

	// Store an Agg proof creation entry in the DB using the start block and the max block
	created, endBlock, err := pa.db.TryCreateAggProofFromSpanProofsLimit(startBlock, maxBlock, l1BlockNumber, l1BlockHash.Hex())
	if err != nil {
		return nil, fmt.Errorf("failed to create agg proof from span proofs: %w", err)
	}

	if created {
		pa.logger.Info("created new AGG proof", "from", startBlock, "to", endBlock)
	} else {
		// Return an error or mock
		if pa.mock {
			return &RequestAggProofResponse{
				StartBlock:     startBlock,
				EndBlock:       endBlock,
				ProofRequestID: "mock_proof_request_id",
			}, nil
		}

		// TODO: Should we return here the proof request ID or the proof?
		return nil, fmt.Errorf("failed to create agg proof from span proofs: already exists")
	}

	// Poll with a ticker for the aggproof request ID creation or the proof itself if it's a mock proof
	preqs := []*ent.ProofRequest{}
	ticker := time.NewTicker(500 * time.Millisecond)
	defer ticker.Stop()
	// End the loop when we find a proof request with status PROVING or COMPLETE
	for len(preqs) <= 0 {
		select {
		case <-ctx.Done():
			return nil, fmt.Errorf("context cancelled")
		case <-ticker.C:
			preqs, err = pa.db.GetProofRequestsWithBlockRange(proofrequest.TypeAGG, startBlock, endBlock)
			if err != nil {
				return nil, fmt.Errorf("failed to get proof request with block range: %w", err)
			}

			// We want to check all but we're only interested in the last one
			for _, preq := range preqs {
				if !pa.mock {
					// If we're not mocking, then we should return the proof request ID
					proverRequestID = preq.ProverRequestID
				}

				switch preq.Status {
				case proofrequest.StatusPROVING, proofrequest.StatusCOMPLETE:
					pa.logger.Info(fmt.Sprintf("agg proof request is %s", preq.Status), "proof_request_id", proverRequestID)
				case proofrequest.StatusFAILED:
					pa.logger.Warn("agg proof request failed", "proof_request_id", preq.ID)
				}
			}
		}
	}

	return &RequestAggProofResponse{
		StartBlock:     startBlock,
		EndBlock:       endBlock,
		ProofRequestID: proverRequestID,
	}, nil
}

func (pa *ProofsAPI) maxBlockL1Limit(ctx context.Context, maxBlock, l1BlockNumber uint64) (uint64, error) {
	// Get the L1 head for the L2 block
	rollupClient, err := dial.DialRollupClientWithTimeout(ctx, dial.DefaultDialTimeout, pa.logger, pa.rollupRPC)
	if err != nil {
		return 0, err
	}

	// If the L1 head corresponding to the maxBlock is higher than the L1 block number,
	// we need to decrease the maxBlock and query the L1 head again until we find an L1 head,
	// that is lower or equal to the L1 block number.
	for maxL1Block := uint64(0); ; maxBlock-- {
		maxL1Block, err = pa.driver.GetL1HeadForL2Block(ctx, rollupClient, maxBlock)
		if err != nil {
			return 0, fmt.Errorf("failed to get l1 head for l2 block: %w", err)
		}
		if maxL1Block <= l1BlockNumber {
			break
		}
	}

	return maxBlock, nil
}
