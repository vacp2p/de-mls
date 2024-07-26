// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

struct UserInfo {
    bytes signaturePubKey;
}

interface IScKeystore {
    function userExists(address user) external view returns (bool);

    function addUser(address user, bytes calldata signaturePubKey) external;

    function getUser(address user) external view returns (UserInfo memory);
}
