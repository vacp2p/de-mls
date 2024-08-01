// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.24;

import { Ownable } from "Openzeppelin/access/Ownable.sol";
import { IScKeystore } from "./IScKeystore.sol";

error UserAlreadyExists();
error UserDoesNotExist();

contract ScKeystore is Ownable, IScKeystore {
    event UserAdded(address user);
    event UserRemoved(address user);

    mapping(address user => bool exists) private users;

    constructor(address initialOwner) Ownable(initialOwner) { }

    function userExists(address user) public view returns (bool) {
        return users[user];
    }

    function addUser(address user) external onlyOwner {
        if (userExists(user)) revert UserAlreadyExists();

        users[user] = true;

        emit UserAdded(user);
    }

    function removeUser(address user) external onlyOwner {
        if (!userExists(user)) revert UserDoesNotExist();
        users[user] == false;
        emit UserRemoved(user);
    }
}
