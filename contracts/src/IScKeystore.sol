// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

interface IScKeystore {
    function userExists(address user) external view returns (bool);

    function addUser(address user) external;

    function removeUser(address user) external;
}
