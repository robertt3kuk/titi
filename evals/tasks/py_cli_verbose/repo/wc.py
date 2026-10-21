import argparse
import sys


def count_words(text):
    return len(text.split())


def main(argv=None):
    parser = argparse.ArgumentParser(description="Count the words in a file.")
    parser.add_argument("path")
    args = parser.parse_args(argv)
    with open(args.path, encoding="utf-8") as handle:
        text = handle.read()
    print(count_words(text))
    return 0


if __name__ == "__main__":
    sys.exit(main())
