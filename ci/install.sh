set -euxo pipefail

main() {
    local target=
    if [ $TRAVIS_OS_NAME = linux ]; then
        target=x86_64-unknown-linux-gnu
        sort=sort

        sudo apt-get update
        sudo apt-get install gcc-arm-none-eabi
    else
        target=x86_64-apple-darwin
        sort=gsort  # for `sort --sort-version`, from brew's coreutils.
    fi

    # install latest `cross` binary
    local tag=$(git ls-remote --tags --refs --exit-code https://github.com/japaric/cross \
                    | cut -d/ -f3 \
                    | grep -E '^v[0.1.0-9.]+$' \
                    | $sort --version-sort \
                    | tail -n1)
    curl -LSfs https://japaric.github.io/trust/install.sh | \
        sh -s -- \
           --force \
           --git japaric/cross \
           --tag $tag \
           --target $target

    # needed to test Xargo
    rustup component add rust-src

    # NOTE(sed) work around for rust-lang/rust#36501
    case $TRAVIS_OS_NAME in
        linux)
            find $(rustc --print sysroot) -name Cargo.toml -print0 | xargs -0 sed -i '/"dylib"/d';
            ;;
        osx)
            find $(rustc --print sysroot) -name Cargo.toml -print0 | xargs -0 sed -i '' '/"dylib"/d';
            ;;
    esac
}

main
