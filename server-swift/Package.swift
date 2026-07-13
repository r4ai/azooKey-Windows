// swift-tools-version: 6.1
// The swift-tools-version declares the minimum version of Swift required to build this package.

import PackageDescription

let package = Package(
    name: "azookey-server",
    products: [
        // Products define the executables and libraries a package produces, making them visible to other packages.
        .library(
            name: "azookey-server",
            type: .dynamic,
            targets: ["azookey-server"]
        ),
        .library(name: "ffi", targets: ["azookey-server"])
    ],
    dependencies: [
        // scripts/patch-kkc.ps1 verifies this exact source before applying the
        // Windows GPU-offload and context-safety patch used by the build.
        .package(
            url: "https://github.com/azookey/AzooKeyKanaKanjiConverter",
            exact: "0.11.2",
            traits: ["Zenzai"]
        )
    ],
    targets: [
        // Targets are the basic building blocks of a package, defining a module or a test suite.
        // Targets can depend on other targets in this package and products from dependencies.
        .target(name: "ffi"),
        .target(
            name: "azookey-server",
            dependencies: [
                .product(name: "KanaKanjiConverterModule", package: "azookeykanakanjiconverter"),
                "ffi"
            ],
            swiftSettings: [
                .interoperabilityMode(.Cxx)
            ]
        ),
        .testTarget(
            name: "azookey-serverTests",
            dependencies: ["azookey-server"],
            swiftSettings: [
                .interoperabilityMode(.Cxx)
            ]
        ),
    ]
)
