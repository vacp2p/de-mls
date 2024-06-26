// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

struct KeyPackage {
    // store serialized onchain
    bytes[] data;
}

struct UserInfo {
    KeyPackage[] keyPackages;
    bytes signaturePubKey;
}

interface IScKeystore {
    function userExists(address user) external view returns (bool);
    function addUser(UserInfo calldata userInfo) external;
    function getUser(address user) external view returns (UserInfo memory);
    function addKeyPackage(KeyPackage calldata) external;
}
