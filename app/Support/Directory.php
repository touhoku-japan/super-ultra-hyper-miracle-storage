<?php
namespace App\Support;


use Exception;

class Directory{


    static function mkFourDir($base_dir,$a,$b,$c,$d){

        $arr=[$a,$b,$c,$d];

        $target=$base_dir;
        foreach($arr as $v){
            $target .= DIRECTORY_SEPARATOR.$v;
            if(false===mkdir($target)){
                return false;
            }
        }
        return $target;
    }

    static function mv_file($filename,$temp_filepath){
        $chars = str_split($filename);//MD5のファイル名

        $count=0;
        $prefix=[];
        $secondary=[];
        $tertiary=[];
        $fourth=[];
        foreach($chars as $c){
            switch($count){
                case 0:
                case 1:
                    $prefix[]=$c;
                    break;
                case 2:
                case 3:
                    $secondary[]=$c;
                    break;
                case 4:
                case 5:
                    $tertiary[]=$c;
                    break;
                case 6:
                case 7:
                    $fourth[]=$c;
                    break;
            }
            if($count===4){
                break;//foreach
            }
            $count++;
        }
        $created_dir=self::mkFourDir(storage_path("main",$prefix,$secondary,$tertiary,$fourth));

        $dest_filepath = $created_dir.DIRECTORY_SEPARATOR.$filename;
        if(false===move_uploaded_file($temp_filepath,$dest_filepath)){
            throw new Exception("move_uploaded_file error");
        }
        return $dest_filepath;

    }

}
