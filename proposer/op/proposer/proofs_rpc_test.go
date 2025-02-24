package proposer

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/ethereum-optimism/optimism/op-service/sources"
	"github.com/ethereum/go-ethereum/log"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/mock"
)

type DriverMock struct {
	mock.Mock
}

func (m *DriverMock) GetL1HeadForL2Block(ctx context.Context, rollupClient *sources.RollupClient, l2End uint64) (uint64, error) {
	args := m.Called(ctx, rollupClient, l2End)
	return args.Get(0).(uint64), args.Error(1)
}

func TestMaxBlockL1Limit(t *testing.T) {
	mockDriver := &DriverMock{}
	logger := log.New()

	// Create a mock HTTP server
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
	}))
	defer server.Close()

	proofsAPI := &ProofsAPI{
		logger:    logger,
		driver:    mockDriver,
		rollupRPC: server.URL,
	}

	ctx := context.Background()
	maxBlock := uint64(100)
	l1BlockNumber := uint64(50)

	t.Run("success", func(t *testing.T) {
		mockDriver.On("GetL1HeadForL2Block", ctx, mock.Anything, maxBlock).Return(uint64(50), nil)

		result, err := proofsAPI.maxBlockL1Limit(ctx, maxBlock, l1BlockNumber)
		assert.NoError(t, err)
		assert.Equal(t, maxBlock, result)
	})

	t.Run("decrease maxBlock", func(t *testing.T) {
		mockDriver.On("GetL1HeadForL2Block", ctx, mock.Anything, maxBlock).Return(uint64(51), nil).Once()
		mockDriver.On("GetL1HeadForL2Block", ctx, mock.Anything, maxBlock-1).Return(uint64(50), nil).Once()

		result, err := proofsAPI.maxBlockL1Limit(ctx, maxBlock, l1BlockNumber)
		assert.NoError(t, err)
		assert.Equal(t, uint64(99), result)
	})

	t.Run("error getting L1 head", func(t *testing.T) {
		mockDriver.On("GetL1HeadForL2Block", ctx, mock.Anything, maxBlock).Return(uint64(0), errors.New("error"))

		result, err := proofsAPI.maxBlockL1Limit(ctx, maxBlock, l1BlockNumber)
		assert.Error(t, err)
		assert.Equal(t, uint64(0), result)
	})
}
