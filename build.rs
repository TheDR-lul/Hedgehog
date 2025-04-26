fn main() {
    #[cfg(windows)]
    embed_resource::compile("hedgehog.rc", embed_resource::NONE);
}