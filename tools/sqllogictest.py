import argparse
import itertools
import os
import sys
import threading
import time
from shutil import copyfile
import subprocess

from generate_big import generate as generate1
from generate_fvecs import generate as generate2
from generate_sort import generate as generate3
from generate_limit import generate as generate4
from generate_aggregate import generate as generate5
from generate_top import generate as generate6
from generate_top_varchar import generate as generate7
from generate_compact import generate as generate8
from generate_hnsw_with_delete import generate as generate9
from generate_index_scan import generate as generate10
from generate_many_import import generate as generate11
from generate_big_point_query_test_fastroughfilter import generate as generate12
from generate_many_import_drop import generate as generate13
from generate_mem_hnsw import generate as generate14
from generate_big_sparse import generate as generate15
from generate_csr import generate as generate16
from generate_bvecs import generate as generate17
from generate_emvb_test_data import generate as generate18
from generate_test_parquet import generate as generate20
from generate_sparse_parquet import generate as generate21
from generate_embedding_parquet import generate as generate22
from generate_varchar_parquet import generate as generate23
from generate_test_parquet import generate as generate24
from generate_tensor_parquet import generate as generate25
from generate_tensor_array_parquet import generate as generate26
from generate_multivector_parquet import generate as generate27
from generate_multivector_knn_scan import generate as generate28
from generate_groupby1 import generate as generate29
from generate_unnest import generate as generate30
from generate_large_import import generate as generate31


class SpinnerThread(threading.Thread):
    def __init__(self):
        super(SpinnerThread, self).__init__()
        self.stop = False

    def run(self):
        spinner = itertools.cycle(["-", "/", "|", "\\"])
        while not self.stop:
            print(next(spinner), end="\r")
            time.sleep(0.1)

    def stop_spinner(self):
        self.stop = True


def process_test(sqllogictest_bin: str, slt_dir: str, data_dir: str, copy_dir: str, test_file_name: str = None,
                 loop: int = 1):
    print("sqlllogictest-bin path is {}".format(sqllogictest_bin))
    print("slt_dir path is {}".format(slt_dir))
    print("data_dir path is {}".format(data_dir))

    test_cnt = 0
    skipped_files = {}
    for i in range(loop):
        print("Start running test loop: {}".format(i + 1))
        for dirpath, dirnames, filenames in os.walk(slt_dir):
            for filename in filenames:
                file = os.path.join(dirpath, filename)

                filename = os.path.basename(file)
                if filename in skipped_files:
                    continue

                if test_file_name is not None and filename != test_file_name:
                    continue

                print("Start running test file: " + file)
                process = subprocess.run(
                    [sqllogictest_bin, file], stdout=subprocess.PIPE, stderr=subprocess.PIPE
                )
                output, error = process.stdout, process.stderr
                print(f"Output: {output.decode()}")  # Prints the output.
                if process.returncode != 0:
                    raise Exception(
                        f"An error occurred: {error.decode()}"
                    )  # Prints the error message.
                print("=" * 99)
                test_cnt += 1
        print("Finish running test loop: {}".format(i + 1))

    print("Finished {} tests.".format(test_cnt))


# copy data
def copy_all(data_dir, copy_dir):
    if not os.path.exists(copy_dir):
        os.makedirs(copy_dir)
    for dirpath, dirnames, filenames in os.walk(data_dir):
        for filename in filenames:
            src_path = os.path.join(dirpath, filename)
            dest_path = os.path.join(copy_dir, filename)
            copyfile(src_path, dest_path)
    print("Finished copying all files.")


# main
if __name__ == "__main__":
    print("Note: this script must be run under root directory of the project.")

    current_path = os.getcwd()

    test_dir = current_path + "/test/sql"
    data_dir = current_path + "/test/data"
    copy_dir = "/var/infinity/test_data"

    parser = argparse.ArgumentParser(description="SQL Logic Test For Infinity")

    parser.add_argument(
        "-g",
        "--generate",
        type=bool,
        default=False,
        dest="generate_if_exists",
    )
    parser.add_argument(
        "-p",
        "--path",
        help="path of sqllogictest-rs",
        type=str,
        default="sqllogictest",
        dest="path",
    )
    parser.add_argument(
        "-t",
        "--test",
        help="path of test directory",
        type=str,
        default=test_dir,
        dest="test",
    )
    parser.add_argument(
        "--test_case",
        help="test_case",
        type=str,
        default=None,
    )
    parser.add_argument(
        "--loop",
        type=int,
        required=False,
        default=1,
    )
    parser.add_argument(
        "-d",
        "--data",
        type=str,
        default=data_dir,
        dest="data",
    )
    parser.add_argument(
        "-c",
        "--copy",
        type=str,
        default=copy_dir,
        dest="copy",
    )
    parser.add_argument(
        "-jc",
        "--just_copy",
        type=bool,
        default=False,
        dest="just_copy_all_data",
    )

    args = parser.parse_args()

    print("Generating file...")
    generate1(args.generate_if_exists, args.copy)
    generate2(args.generate_if_exists, args.copy)
    generate3(args.generate_if_exists, args.copy)
    generate4(args.generate_if_exists, args.copy)
    generate5(args.generate_if_exists, args.copy)
    generate6(args.generate_if_exists, args.copy)
    generate7(args.generate_if_exists, args.copy)
    generate8(args.generate_if_exists, args.copy)
    generate9(args.generate_if_exists, args.copy)
    generate10(args.generate_if_exists, args.copy)
    generate11(args.generate_if_exists, args.copy)
    generate12(args.generate_if_exists, args.copy)
    generate13(args.generate_if_exists, args.copy)
    generate14(args.generate_if_exists, args.copy)
    generate15(args.generate_if_exists, args.copy)
    generate16(args.generate_if_exists, args.copy)
    generate17(args.generate_if_exists, args.copy)
    generate18(args.generate_if_exists, args.copy)
    # generate19(args.generate_if_exists, args.copy)
    # generate_wiki_embedding.generate()
    generate20(args.generate_if_exists, args.copy)
    generate21(args.generate_if_exists, args.copy)
    generate22(args.generate_if_exists, args.copy)
    generate23(args.generate_if_exists, args.copy)
    generate24(args.generate_if_exists, args.copy)
    generate25(args.generate_if_exists, args.copy)
    generate26(args.generate_if_exists, args.copy)
    generate27(args.generate_if_exists, args.copy)
    generate28(args.generate_if_exists, args.copy)
    generate29(args.generate_if_exists, args.copy)
    generate30(args.generate_if_exists, args.copy)
    generate31(args.generate_if_exists, args.copy)

    print("Generate file finshed.")

    # remove all file in tmp directory and create an empty tmp
    if os.path.exists("/var/infinity/test_data/tmp"):
        os.system("rm -rf /var/infinity/test_data/tmp")
    os.makedirs("/var/infinity/test_data/tmp")

    print("Start copying data...")
    if args.just_copy_all_data is True:
        copy_all(args.data, args.copy)
    else:
        copy_all(args.data, args.copy)
        print("Start testing...")
        start = time.time()
        try:
            process_test(args.path, args.test, args.data, args.copy, args.test_case, args.loop)
        except Exception as e:
            print(e)
            sys.exit(-1)
        end = time.time()
        print("Test finished.")
        print("Time cost: {}s".format(end - start))
