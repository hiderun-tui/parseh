package keeper

import "errors"

var (
	ErrInvalidProvider = errors.New("inference: provider has no address")
	ErrJobNotFound     = errors.New("inference: job not found")
)
