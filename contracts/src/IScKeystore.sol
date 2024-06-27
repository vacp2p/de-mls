// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

struct KeyPackage {
    // store serialized onchain
    bytes[] data;
}

struct UserInfo {
    uint256[] keyPackageIndices;
    bytes signaturePubKey;
}

interface IScKeystore {
    function userExists(address user) external view returns (bool);
    function addUser(bytes calldata signaturePubKey, KeyPackage calldata keyPackage) external;
    function getUser(address user) external view returns (UserInfo memory);
    function addKeyPackage(KeyPackage calldata) external;
    function getAvailableKeyPackage(address user) external view returns (KeyPackage memory);
}
