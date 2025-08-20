let public_env = {};
function set_private_env(environment) {
}
function set_public_env(environment) {
  public_env = environment;
}
export {
  set_private_env as a,
  public_env as p,
  set_public_env as s
};
