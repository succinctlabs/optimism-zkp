// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

////////////////////////////////////////////////////////////////
//            `OPSuccinctFaultDisputeGame` Errors             //
////////////////////////////////////////////////////////////////

/// @notice Thrown when the claim has already been challenged.
error ClaimAlreadyChallenged();

/// @notice Thrown when the game type of the parent game does not match the current game.
error UnexpectedGameType();

/// @notice Thrown when the parent game is invalid.
error InvalidParentGame();

/// @notice Thrown when the parent game is not resolved.
error ParentGameNotResolved();

/// @notice Thrown when the claim has already been proven.
error AlreadyProven();

/// @notice Thrown when the credit transfer fails.
error CreditTransferFailed();

/// @notice Thrown when the user is not whitelisted.
error NotWhitelisted();

/// @notice Thrown when actions are attempted without going through the entry point.
error NotThroughEntryPoint();
