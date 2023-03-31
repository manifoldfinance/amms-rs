//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IERC4626Vault {
    function asset() external view returns (address);
    function decimals() external view returns (uint8);
    function totalSupply() external view returns (uint256);
    function totalAssets() external view returns (uint256);
}

interface IERC20 {
    function decimals() external view returns (uint8);
}

/**
 @dev This contract is not meant to be deployed. Instead, use a static call with the
      deployment bytecode as payload.
 */
contract GetERC4626VaultDataBatchRequest {
    struct VaultData {
        address vaultToken;
        uint8 vaultTokenDecimals;
        address assetToken;
        uint8 assetTokenDecimals;
        uint256 vaultTokenReserve;
        uint256 assetTokenReserve;
    }

    constructor(address[] memory vaults) {
        VaultData[] memory allVaultData = new VaultData[](vaults.length);

        for (uint256 i = 0; i < vaults.length; ++i) {
            address vaultAddress = vaults[i];

            if (codeSizeIsZero(vaultAddress)) continue;

            VaultData memory vaultData;

            // Get tokens
            vaultData.vaultToken = vaultAddress;
            vaultData.assetToken = IERC4626Vault(vaultAddress).asset();

            // Check that assetToken exists
            if (codeSizeIsZero(vaultData.assetToken)) continue;

            // Get vault token decimals
            vaultData.vaultTokenDecimals = IERC4626Vault(vaultAddress).decimals();

            // Get asset token decimals
            (
                bool assetTokenDecimalsSuccess,
                bytes memory assetTokenDecimalsData
            ) = vaultData.assetToken.call(abi.encodeWithSignature("decimals()"));

            if (assetTokenDecimalsSuccess) {
                uint256 assetTokenDecimals;

                if (assetTokenDecimalsData.length == 32) {
                    (assetTokenDecimals) = abi.decode(
                        assetTokenDecimalsData,
                        (uint256)
                    );

                    if (assetTokenDecimals == 0 || assetTokenDecimals > 255) {
                        continue;
                    } else {
                        vaultData.assetTokenDecimals = uint8(assetTokenDecimals);
                    }
                } else {
                    continue;
                }
            } else {
                continue;
            }

            // Get token reserves
            vaultData.vaultTokenReserve = IERC4626Vault(vaultAddress).totalSupply();
            vaultData.assetTokenReserve = IERC4626Vault(vaultAddress).totalAssets();

            allVaultData[i] = vaultData;
        }

        // ensure abi encoding, not needed here but increase reusability for different return types
        // note: abi.encode add a first 32 bytes word with the address of the original data
        bytes memory _abiEncodedData = abi.encode(allVaultData);

        assembly {
            // Return from the start of the data (discarding the original data address)
            // up to the end of the memory used
            let dataStart := add(_abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }

    function codeSizeIsZero(address target) internal view returns (bool) {
        if (target.code.length == 0) {
            return true;
        } else {
            return false;
        }
    }
}