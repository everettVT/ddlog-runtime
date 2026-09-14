//! Every upstream fixture byte is pinned. Serialized types that gain fields must
//! do so without changing these files or the records they decode to.
use sha2::{Digest, Sha256};
use std::path::Path;

const PINNED: &[(&str, &str)] = &[
        ("initialize.json", "9825c0897f45b7430dc18de6f796a37007737175fc7961fbf32c68843348c1ce"),
        ("manifest.json", "7b2101a5a311956c0a36f14f04154e6625b04d41402e2aeb33700e9ea2b97958"),
        ("nested-program.dl", "9f31f9f34970ec7bf0d4a87a352d50491c632f00811ee7e45b127eebeecfbe2d"),
        ("operations.json", "14e221cf640e6db56795f2fa7927eae2c938029bd78bac0f10b1b3421f0ec49e"),
        ("records.json", "50182c20b9c69e5a07cea2d0ec4745045a00589ac3ceb88412e1179eb12ad203"),
        ("registry/processor_94a3652e1247018b2ba3249e8945e936/current.json", "1bcaee21af6967ac6882a6db0ed7034e20dbe8a13728e7155949bc755a9fafa3"),
        ("registry/processor_94a3652e1247018b2ba3249e8945e936/versions/0a9c8173319f8107f9ad2c37446de12021176145edc6c7fbe2862dfac4a9c657.json", "3b75b619e6048b9fb5a0f13bc3fe4603e02510ff9c379c9d4903e31737beb18c"),
        ("registry/processor_a5061511ae805a164da144671ba62ec2/current.json", "73a05252243458c4deba292a13f79f35513e671ee666b32588f630ca55b19955"),
        ("registry/processor_a5061511ae805a164da144671ba62ec2/versions/3dbc92487f05ba94199713e681f2ed05ecacae35cc3f62c86fcfb56281d0fad1.json", "ea26bb40ebce18f63a91cb714dd548318fe5866bda79bbc38e8f9717dd16ca83"),
        ("registry/processor_d27e6d76c6703fc3d498f7472356ddf4/current.json", "c42084cf1d5f5eb1d44de0f2b658503ec9845d49d8ab2ca227e888d4fa3636f3"),
        ("registry/processor_d27e6d76c6703fc3d498f7472356ddf4/versions/c4779c5af7b16400c744cbcf06fdfab52894d04292230389a0899b99d5b2cb52.json", "12df53020e7da6fdbfab416573d161098f44c0548173cbef630efbd48fb435cc"),
        ("registry/processor_e6952bf74116c7496625b9011b33742d/current.json", "244f3b8917fe4901981a89afb729f1c36bf2a9badc79feab23ec5664cef2e3b5"),
        ("registry/processor_e6952bf74116c7496625b9011b33742d/versions/9837a519f04b5ce0182d6492e6d322ef6ad61d79bc1fea857912d02ce9d87c1c.json", "69f4676bdbf4fe64db3967d37dbc5be931d7f8f8a17a0cab62dfaab0d99ae15b"),
        ("registry/processor_f73af36626b286b84d4d7ebf3ec6becf/current.json", "8075c1d95ada7d90745d29831b19915b371dfb489526eecc3c8b8c527f5dab71"),
        ("registry/processor_f73af36626b286b84d4d7ebf3ec6becf/versions/75e4f50eae99a217292803f8bb9c41752fd1ecd193c3ebd5b6a0568ca9b4a226.json", "a7b31ad319a3adb927eb231d7610349c865b7b886dc4fa61406ea0814af2c380"),
        ("tools.json", "3b0d126d1ad26bcadcb17cb449225d3a82f1470cec4aba4511bb1fc6538aab9d"),
];

fn walk(root: &Path, directory: &Path, found: &mut Vec<String>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(root, &path, found);
        } else {
            found.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
}

#[test]
fn upstream_fixture_hashes_are_unchanged() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/upstream");
    let mut found = Vec::new();
    walk(&root, &root, &mut found);
    found.sort();
    let pinned: Vec<&str> = PINNED.iter().map(|(name, _)| *name).collect();
    assert_eq!(found, pinned, "fixture inventory changed");
    for (name, expected) in PINNED {
        let actual = format!(
            "{:x}",
            Sha256::digest(std::fs::read(root.join(name)).unwrap())
        );
        assert_eq!(actual, *expected, "{name} changed");
    }
}
