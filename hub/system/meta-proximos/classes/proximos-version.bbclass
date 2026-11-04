PROXIMOS_VERSION ??= ""

BB_DONT_CACHE = "1"

def proximos_version_from_git(d):
    """Return `git describe` for the monorepo, or fail the parse."""
    import os
    import bb.process

    layerdir = d.getVar('PROXIMOS_LAYERDIR')
    if not layerdir:
        bb.fatal("proximos-version: PROXIMOS_LAYERDIR is unset. It is defined "
                 "by meta-proximos/conf/layer.conf — is the layer in "
                 "BBLAYERS?")

    cmd = ['git', '-C', layerdir, 'describe', '--tags', '--always', '--dirty']
    env = dict(os.environ)
    env['GIT_OPTIONAL_LOCKS'] = '0'
    try:
        version, _ = bb.process.run(cmd, env=env)
    except bb.process.CmdError as exc:
        bb.fatal("proximos-version: cannot determine the build version.\n"
                 "  command: %s\n"
                 "  error:   %s\n"
                 "Fix one of these:\n"
                 "  * build from a git checkout of the monorepo, or\n"
                 "  * export PROXIMOS_VERSION=<version> before invoking kas, "
                 "or\n"
                 "  * if the checkout belongs to another user (containers): "
                 "git config --global --add safe.directory '*'"
                 % (' '.join(cmd), exc))

    version = version.strip()
    if not version:
        bb.fatal("proximos-version: `%s` returned nothing."
                 % ' '.join(cmd))
    return version

python __anonymous() {
    version = (d.getVar('PROXIMOS_VERSION') or '').strip()
    if not version:
        version = proximos_version_from_git(d)
    d.setVar('PROXIMOS_VERSION', version)
    d.setVarFlag('PROXIMOS_VERSION', 'vardepvalue', version)
}
