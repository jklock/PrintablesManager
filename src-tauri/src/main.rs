fn main() {
    if std::env::args().skip(1).any(|arg| arg == "--selftest") {
        printables_manager::selftest();
        return;
    }
    printables_manager::run()
}
