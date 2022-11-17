<?php

namespace App\Http\Controllers;

use App\Support\Directory;
use Exception;
use Illuminate\Http\Exceptions\ThrottleRequestsException;
use Illuminate\Http\Request;
use Illuminate\Support\Facades\DB;
use Throwable;

class ApiController extends Controller
{
    //

    function __construct()
    {

    }

    function list_download(Request $req)
    {

    }

    function upload(Request $req)
    {
        $file = $req->file("file");
        $temp_filename = md5(time() . mt_rand());

        $temp_filepath = "temporary" . DIRECTORY_SEPARATOR . $temp_filename;
        try {
            $file->storeAs("temporary", $temp_filename);
            $filename = md5_file(storage_path($temp_filepath));

            $dest_filepath = Directory::mv_file($filename, $temp_filename);

            DB::table("files")->insert([
                "name_md5" => $filename
                , "file_path" => $dest_filepath
            ]);

        } catch (Throwable $t) {
            unlink($temp_filepath);
        }
    }

    function download(Request $req)
    {
        $all = $req->all();
        $result = [
            "result" => false
            , "message" => ""
        ];
        if (!is_integer($all['id'])) {
            $result["message"] = "idが正しい値ではありませんでした";
            return response()->json($result, 500);
        }
    }


    function find_md5(Request $req)
    {
        $all = $req->all();
        $result = [
            "result" => false
            , "message" => ""
        ];
        $first = DB::table("files")->select("*")->where("name_md5", $all['md5'])->first();
        if (null === $first) {
            $result["message"] = "指定のMD5のデータは存在しませんでした。";
            return response()->json($result, 404);
        }

        $fs = filesize(storage_path($first->file_path));
        if (false === $fs) {
            $result["message"] = "ファイルサイズを取得できませんでした。";
            return response()->json($result, 200);
        }

        $result['result'] = true;
        $result['info'] = [
            "md5" => $first->name_md5
            , "filesize" => $fs
            , "created_at" => $first->created_at
            , "updated_at" => $first->updated_at
        ];

        return response()->json($result);

    }
}
