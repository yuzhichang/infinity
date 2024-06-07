// Copyright(C) 2023 InfiniFlow, Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#include "unit_test/base_test.h"

import stl;
import term;
import chinese_analyzer;
using namespace infinity;

namespace fs = std::filesystem;

class ChineseAnalyzerTest : public BaseTest {};

TEST_F(ChineseAnalyzerTest, test1) {
    // Get the path to the executable using the /proc/self/exe symlink
    fs::path executablePath = "/proc/self/exe";
    std::error_code ec;
    // Resolve the symlink to get the actual path
    executablePath = fs::canonical(executablePath, ec);
    if (ec) {
        std::cerr << "Error resolving the path: " << executablePath << " " << ec.message() << std::endl;
        return;
    }
    std::cerr << "/proc/self/exe: " << executablePath << std::endl;

    fs::path ROOT_PATH = executablePath.parent_path().parent_path().parent_path().parent_path() / "resource";
    std::cerr << "ROOT_PATH: " << ROOT_PATH << std::endl;

    if (!fs::exists(ROOT_PATH)) {
        std::cerr << "Resource directory doesn't exist: " << ROOT_PATH << std::endl;
        return;
    }

    ChineseAnalyzer analyzer(ROOT_PATH.string());
    analyzer.Load();
    ChineseAnalyzer analyzer2(analyzer);
    Vector<String> queries = {"南京市长江大桥，。。", "graphic card", "graphics card"};

    for (auto &query : queries) {
        TermList term_list;
        analyzer.Analyze(query, term_list);
        std::cout << "Text #" << query << "# parsed as:" << std::endl;
        for (unsigned i = 0; i < term_list.size(); ++i) {
            std::cout << "\t" << i << "#" << term_list[i].text_ << "@" << term_list[i].word_offset_ << "#";
        }
        std::cout << std::endl;

        TermList term_list2;
        analyzer2.Analyze(query, term_list2);
        for (unsigned i = 0; i < term_list2.size(); ++i) {
            std::cout << "\t" << i << "#" << term_list[i].text_ << "@" << term_list[i].word_offset_ << "#";
        }
        std::cout << std::endl;
    }
}
